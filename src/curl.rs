/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::error::Error as StdError;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::progress::display_progress;
use crate::util::display::CommandDisplay;
use crate::util::display::CommandStderrDisplay;
use crate::util::http_status::HttpStatus;

const NUM_RETRYABLE_CURL_MAX_ATTEMPTS: u8 = 3;
const NUM_TRANSIENT_ERROR_CURL_MAX_ATTEMPTS: u64 = 3;

/// Specify a custom user-agent when making requests. In the unfortunate event
/// that a site hosting an artifact gets overloaded with requests, hopefully
/// this will help them identify whether DotSlash is involed.
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; DotSlash/",
    env!("CARGO_PKG_VERSION"),
    "; +",
    env!("CARGO_PKG_HOMEPAGE"),
    ")"
);

// curl exit codes: https://man.cx/curl#heading10
const CURL_RETRYABLE_EXIT_CODES: &[i32] = &[
    18, // Partial file. Only a part of the file was transferred.
    28, // Operation timeout.
    56, // Failure in receiving network data.
    // New set of retryable codes, after infra switched to http2.
    7,  // CURLE_COULDNT_CONNECT
    16, // CURLE_HTTP2
    32, // CURLE_WRITE_ERROR
    92, // CURLE_HTTP2_STREAM
];

// HTTP page not retrieved. The requested url was not found or returned
// another error with the HTTP error code being 400 or above.
// This return code only appears if -f, --fail is used.
// https://curl.haxx.se/docs/manpage.html#22
// https://curl.haxx.se/docs/manpage.html#-f
const CURL_HTTP_RETURNED_ERROR_EXIT_CODE: i32 = 22;

enum CurlRequestType<'a> {
    /// String is the argument to use with --output.
    Get(&'a str),
}

pub struct FetchContext<'a> {
    pub artifact_name: &'a str,
    pub content_length: u64,
    pub show_progress: bool,
}

#[derive(Debug, Error)]
pub enum CurlError {
    // Spawning `curl` failed. `curl` is missing or is not executable.
    #[error("failed to execute `{0}`")]
    Execute(DebugCommand, #[source] io::Error),

    // `curl` completed with some exit code that is not 22.
    #[error("`{0}`")]
    CurlExit(DebugCommand, #[source] CurlExit),

    // `curl` completed with exit code 22. The server returned 4xx or 5xxx.
    #[error("`{0}`")]
    HttpStatus(DebugCommand, #[source] HttpStatus),

    // DotSlash failed managing the thread responsible for displaying a progress
    // indicator.
    #[error("progress indicator thread panicked with `{0}`")]
    JoinProgressThread(String),
}

#[derive(Debug)]
pub struct CurlExit(Output);

#[derive(Debug)]
pub struct CurlCommand<'a> {
    url: &'a OsStr,
    retry: u64,
}

#[derive(Debug)]
pub struct DebugCommand(Command);

impl From<&Command> for DebugCommand {
    fn from(command: &Command) -> DebugCommand {
        let mut clone = Command::new(command.get_program());
        clone.args(command.get_args());
        DebugCommand(clone)
    }
}

impl fmt::Display for DebugCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&CommandDisplay::new(&self.0), f)
    }
}

impl fmt::Display for CurlExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // curl's stderr is pretty well formatted so avoid adding noise
        // when we know it's already useful.
        if self.0.stderr.starts_with(b"curl: (") {
            write!(f, "{}", String::from_utf8_lossy(&self.0.stderr).trim_end())
        } else {
            fmt::Display::fmt(&CommandStderrDisplay::new(&self.0), f)
        }
    }
}

impl StdError for CurlExit {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        // TODO: Provide hints for different kinds of errors.
        None
    }
}

impl CurlError {
    fn from_command_error(command: &Command, error: io::Error) -> CurlError {
        CurlError::Execute(command.into(), error)
    }

    fn from_command_output(command: &Command, output: Output) -> CurlError {
        if output.status.code() == Some(CURL_HTTP_RETURNED_ERROR_EXIT_CODE) {
            if let Some(http_status) = parse_http_returned_error(&output.stderr) {
                return CurlError::HttpStatus(command.into(), HttpStatus::from(http_status));
            };
        }

        CurlError::CurlExit(command.into(), CurlExit(output))
    }

    fn is_retryable(&self) -> bool {
        if let Self::CurlExit(_, source) = self {
            if let Some(exit_code) = source.0.status.code() {
                return CURL_RETRYABLE_EXIT_CODES.contains(&exit_code);
            }
        }
        false
    }

    fn is_too_many_requests(&self) -> bool {
        matches!(self, Self::HttpStatus(_, HttpStatus::TooManyRequests))
    }
}

impl CurlCommand<'_> {
    pub fn new(url: &OsStr) -> CurlCommand {
        CurlCommand {
            url,
            retry: NUM_TRANSIENT_ERROR_CURL_MAX_ATTEMPTS,
        }
    }

    pub fn get_request(&self, target: &Path, context: &FetchContext) -> Result<(), CurlError> {
        // Because `target` is ultimately used with Command.args(), we should make
        // it possible to use a non-utf8 value, as unlikely as it is, in practice.
        let output_arg = target.to_str().unwrap();

        // TODO: Implement a progress handler that prints dots so long as the curl
        // request is still running. For now, just treat it as if the user had not
        // requested a progress bar.

        // While making the request, poll the target and report what percentage
        // done it is compared to content_length.
        let handler = if context.show_progress {
            eprintln!("Downloading {}...", context.artifact_name);
            Some(display_progress(context.content_length, target))
        } else {
            None
        };

        // If the request fails, the `progress_sender` channel is dropped,
        // and the progress thread uses this to finish by itself.
        self.make_request(&mut self.curl_command(self.url, &CurlRequestType::Get(output_arg)))?;

        if let Some((progress_sender, join_handler)) = handler {
            // Let the progress thread know that we're done done.
            // It may already know this if the content size information
            // is correct.
            let _ = progress_sender.send(());
            join_handler
                .join()
                .map_err(|e| CurlError::JoinProgressThread(format!("{:?}", e)))?;
        }

        Ok(())
    }

    fn make_request(&self, curl_command: &mut Command) -> Result<Vec<u8>, CurlError> {
        let mut retries = 1..=NUM_RETRYABLE_CURL_MAX_ATTEMPTS;
        loop {
            let output = match curl_command.output() {
                Ok(output) => output,
                // If curl failed to execute, exit immediately.
                Err(e) => return Err(CurlError::from_command_error(curl_command, e)),
            };

            if output.status.success() {
                // curl completed successfully!
                return Ok(output.stdout);
            }

            let curl_error = CurlError::from_command_output(curl_command, output);

            // Sometimes we manually retry ourselves...
            if let Some(retry_num) = retries.next() {
                // curl failed, but it satisfies our "retryable" heuristic,
                // so loop again.
                if curl_error.is_retryable() {
                    continue;
                }

                // curl failed, but we're hitting the server too hard,
                // so loop again, but wait a little bit.
                if curl_error.is_too_many_requests() {
                    // 1^3=1s  ->  2^3=8s  ->  3^3=27s
                    thread::sleep(Duration::from_secs(retry_num.pow(3) as u64));
                    continue;
                }
            }

            return Err(curl_error);
        }
    }

    fn curl_command(&self, url: &OsStr, request_type: &CurlRequestType) -> Command {
        let mut curl_command = Command::new("curl");

        // https://cygwin.com/cygwin-ug-net/using-cygwinenv.html
        if cfg!(windows) {
            curl_command.env("CYGWIN", "noglob").env("MSYS", "noglob");
        }

        // Follow redirects.
        curl_command.arg("--location");

        //
        // https://curl.haxx.se/docs/manpage.html
        //

        // If a transient error is returned when curl tries to perform a
        // transfer, it will retry this number of times before giving up.
        curl_command.arg("--retry");
        curl_command.arg(self.retry.to_string());

        // When an HTTP server fails to deliver a document, it returns an
        // HTML document stating so. This flag will prevent curl from
        // outputting that and return error 22.
        // (In other words, fail on 404 - expired or bad handle)
        curl_command.arg("--fail");

        // Silent or quiet mode. Don't show progress meter or error messages.
        // Makes Curl mute.
        curl_command.arg("--silent");

        // When used with -s, --silent, it makes curl show an error message if
        // it fails.
        curl_command.arg("--show-error");

        curl_command.arg("--user-agent");
        curl_command.arg(USER_AGENT);

        curl_command.arg(url);

        match request_type {
            CurlRequestType::Get(output) => {
                curl_command.args(["--output", output]);
            }
        }

        curl_command
    }
}

fn parse_http_returned_error(stderr: &[u8]) -> Option<usize> {
    // "The requested URL returned error: %d\n" (or \r\n on Windows)
    // https://github.com/curl/curl/blob/eab2f95c0de9/lib/http.c#L627-L630
    //
    // Older versions include a "reason" string after the code for HTTP/1
    // requests. https://github.com/curl/curl/issues/12159
    //
    // `--retry` may cause the error to be repeated multiple times.
    // Only look at the first one.
    std::str::from_utf8(stderr)
        .ok()?
        .lines()
        .next()?
        .trim_start_matches("curl: (22) The requested URL returned error: ")
        .split_ascii_whitespace()
        .next()?
        .parse::<usize>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_returned_error() {
        assert_eq!(
            parse_http_returned_error(b"curl: (22) The requested URL returned error: 429"),
            Some(429),
        );
        assert_eq!(
            parse_http_returned_error(b"curl: (22) The requested URL returned error: 429\n"),
            Some(429),
        );
        assert_eq!(
            parse_http_returned_error(b"curl: (22) The requested URL returned error: 429\r\n"),
            Some(429),
        );
        assert_eq!(
            parse_http_returned_error(
                b"curl: (22) The requested URL returned error: 429 Too Many Requests\n",
            ),
            Some(429),
        );
        assert_eq!(
            parse_http_returned_error(b"curl: (22) The requested URL returned error: %d\n"),
            None,
        );
    }

    #[test]
    fn user_agent() {
        let version = env!("CARGO_PKG_VERSION");
        // For reference, the user-agent that Google sends from its webcrawler is:
        //
        //     Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)
        //
        // The purpose of this test is to make it easier to visually verify that
        // the user-agent is what we expect.
        assert_eq!(
            format!("Mozilla/5.0 (compatible; DotSlash/{version}; +https://dotslash-cli.com)"),
            USER_AGENT
        );
    }
}
