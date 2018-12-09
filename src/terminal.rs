// Copyright © 2016 Felix Obenhuber
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use crate::profiles::*;
use crate::record::{Format, Level, Record};
use crate::utils::config_get;
use crate::utils::terminal_width;
use crate::LogSink;
use clap::{values_t, ArgMatches};
use failure::{err_msg, format_err, Error};
use futures::future::{lazy, Future};
use futures::{Async, AsyncSink, Poll, Sink, StartSend};
use regex::Regex;
use std::cmp::{max, min};
use std::io::Write;
use std::str::FromStr;
use termcolor::{Buffer, BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};
use tokio::io;

const DIMM_COLOR: Color = Color::Ansi256(243);

/// Construct a terminal sink for format from args with give profile
pub fn try_from<'a>(args: &ArgMatches<'a>, profile: &Profile) -> Result<LogSink, Error> {
    let format = args
        .value_of("format")
        .ok_or_else(|| format_err!("Missing format argument"))
        .and_then(|f| Format::from_str(f).map_err(err_msg))
        .unwrap_or(Format::Human);

    let sink = match format {
        Format::Human => Box::new(Human::from(args, profile, format)) as LogSink,
        format => Box::new(format) as LogSink,
    };

    Ok(Box::new(sink.sink_map_err(|e| {
        failure::format_err!("Terminal error: {}", e)
    })))
}

impl Sink for Format {
    type SinkItem = Record;
    type SinkError = Error;

    fn start_send(&mut self, record: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let format = self.clone();
        let f = lazy(move || record.format(&format))
            .and_then(|mut l| {
                l.push_str("\n");
                io::write_all(io::stdout(), l).map_err(|e| e.into())
            })
            .map_err(|e| panic!(e))
            .map(|_| ());
        tokio::spawn(f);
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}

/// Human readable terminal output
struct Human {
    writer: BufferWriter,
    date_format: Option<(&'static str, usize)>,
    highlight: Vec<Regex>,
    process_width: usize,
    tag_width: Option<usize>,
    thread_width: usize,
    dimm_color: Option<Color>,
}

impl Human {
    pub fn from<'a>(args: &ArgMatches<'a>, profile: &Profile, _: Format) -> Human {
        let mut hl = profile.highlight.clone();
        if args.is_present("highlight") {
            hl.extend(values_t!(args.values_of("highlight"), String).unwrap());
        }
        let highlight = hl.iter().flat_map(|h| Regex::new(h)).collect();

        let color = {
            match args
                .value_of("color")
                .unwrap_or_else(|| config_get("terminal_color").unwrap_or_else(|| "auto"))
            {
                "always" => ColorChoice::Always,
                "never" => ColorChoice::Never,
                "auto" | _ => ColorChoice::Auto,
            }
        };
        let no_dimm = args.is_present("no_dimm") || config_get("terminal_no_dimm").unwrap_or(false);
        let tag_width = config_get("terminal_tag_width");
        let hide_timestamp = args.is_present("hide_timestamp")
            || config_get("terminal_hide_timestamp").unwrap_or(false);
        let show_date =
            args.is_present("show_date") || config_get("terminal_show_date").unwrap_or(false);
        let date_format = if show_date {
            if hide_timestamp {
                Some(("%m-%d", 5))
            } else {
                Some(("%m-%d %H:%M:%S.%f", 12 + 1 + 5))
            }
        } else if hide_timestamp {
            None
        } else {
            Some(("%H:%M:%S.%f", 12))
        };

        Human {
            writer: BufferWriter::stdout(color),
            dimm_color: if no_dimm { None } else { Some(DIMM_COLOR) },
            highlight,
            date_format,
            tag_width,
            process_width: 0,
            thread_width: 0,
        }
    }

    // Dynamic tag width estimation according to terminal width
    fn tag_width(&self) -> usize {
        let terminal_width = terminal_width();
        self.tag_width.unwrap_or_else(|| match terminal_width {
            Some(n) if n <= 80 => 15,
            Some(n) if n <= 90 => 20,
            Some(n) if n <= 100 => 25,
            Some(n) if n <= 110 => 30,
            _ => 35,
        })
    }

    #[cfg(target_os = "windows")]
    fn hashed_color(i: &str) -> Color {
        let v = i.bytes().fold(42u8, |c, x| c ^ x) % 7;
        match v {
            0 => Color::Blue,
            1 => Color::Green,
            2 => Color::Red,
            3 => Color::Cyan,
            4 => Color::Magenta,
            5 => Color::Yellow,
            _ => Color::White,
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn hashed_color(i: &str) -> Color {
        // Some colors are hard to read on (at least) dark terminals
        // and I consider some others as ugly.
        Color::Ansi256(match i.bytes().fold(42u8, |c, x| c ^ x) {
            c @ 0...1 => c + 2,
            c @ 16...21 => c + 6,
            c @ 52...55 | c @ 126...129 => c + 4,
            c @ 163...165 | c @ 200...201 => c + 3,
            c @ 207 => c + 1,
            c @ 232...240 => c + 9,
            c => c,
        })
    }

    fn print(&mut self, record: &Record) -> Result<(), Error> {
        let timestamp = if let Some((format, len)) = self.date_format {
            if let Some(ref ts) = record.timestamp {
                let mut ts = time::strftime(format, &ts).expect("Date format error");
                ts.truncate(len);
                ts
            } else {
                " ".repeat(len)
            }
        } else {
            String::new()
        };

        let tag_width = self.tag_width();
        let tag_chars = record.tag.chars().count();
        let tag = format!(
            "{:>width$}",
            record
                .tag
                .chars()
                .take(min(tag_width, tag_chars))
                .collect::<String>(),
            width = tag_width
        );

        self.process_width = max(self.process_width, record.process.chars().count());
        let pid = if record.process.is_empty() {
            " ".repeat(self.process_width)
        } else {
            format!("{:<width$}", record.process, width = self.process_width)
        };
        self.thread_width = max(self.thread_width, record.thread.chars().count());
        let tid = if !record.thread.is_empty() {
            format!(" {:>width$}", record.thread, width = self.thread_width)
        } else {
            String::new()
        };

        let highlight = !self.highlight.is_empty()
            && (self.highlight.iter().any(|r| r.is_match(&record.tag))
                || self.highlight.iter().any(|r| r.is_match(&record.message)));

        let preamble_width = timestamp.chars().count()
            + 1 // " "
            + tag.chars().count()
            + 2 // " ("
            + pid.chars().count() + tid.chars().count()
            + 2 // ") "
            + 3; // level

        let timestamp_color = if highlight {
            Some(Color::Yellow)
        } else {
            self.dimm_color
        };
        let tag_color = Self::hashed_color(&record.tag);
        let pid_color = Self::hashed_color(&pid);
        let tid_color = Self::hashed_color(&tid);
        let level_color = match record.level {
            Level::Info => Some(Color::Green),
            Level::Warn => Some(Color::Yellow),
            Level::Error | Level::Fatal | Level::Assert => Some(Color::Red),
            _ => self.dimm_color,
        };

        let write_preamble = |buffer: &mut Buffer| -> Result<(), Error> {
            let mut spec = ColorSpec::new();
            buffer.set_color(spec.set_fg(timestamp_color))?;
            buffer.write_all(timestamp.as_bytes())?;
            buffer.write_all(b" ")?;

            buffer.set_color(spec.set_fg(Some(tag_color)))?;
            buffer.write_all(tag.as_bytes())?;
            buffer.set_color(spec.set_fg(None))?;

            buffer.write_all(b" (")?;
            buffer.set_color(spec.set_fg(Some(pid_color)))?;
            buffer.write_all(pid.as_bytes())?;
            if !tid.is_empty() {
                buffer.set_color(spec.set_fg(Some(tid_color)))?;
                buffer.write_all(tid.as_bytes())?;
            }
            buffer.set_color(spec.set_fg(None))?;
            buffer.write_all(b") ")?;

            buffer.set_color(
                spec.set_bg(level_color)
                    .set_fg(level_color.map(|_| Color::Black)), // Set fg only if bg is set
            )?;
            write!(buffer, " {} ", record.level)?;
            buffer.set_color(&ColorSpec::new())?;

            Ok(())
        };

        let payload_len = terminal_width().unwrap_or(std::usize::MAX) - preamble_width - 3;
        let message = &record.message;
        let message_len = message.chars().count();
        let chunks = message_len / payload_len + 1;

        let mut buffer = self.writer.buffer();

        for i in 0..chunks {
            write_preamble(&mut buffer)?;

            let c = if chunks == 1 {
                "   "
            } else if i == 0 {
                " ┌ "
            } else if i == chunks - 1 {
                " └ "
            } else {
                " ├ "
            };

            buffer.write_all(c.as_bytes())?;

            let chunk = message
                .chars()
                .skip(i * payload_len)
                .take(payload_len)
                .collect::<String>();
            buffer.set_color(ColorSpec::new().set_fg(level_color))?;
            buffer.write_all(chunk.as_bytes())?;
            buffer.write_all(b"\n")?;
        }

        buffer
            .set_color(&ColorSpec::new())
            .and_then(|_| self.writer.print(&buffer))
            .map_err(|e| e.into())
    }
}

impl Sink for Human {
    type SinkItem = Record;
    type SinkError = Error;

    fn start_send(&mut self, record: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.print(&record).map(|_| AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
