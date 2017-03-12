// Copyright © 2016 Felix Obenhuber
// This program is free software. It comes without any warranty, to the extent
// permitted by applicable law. You can redistribute it and/or modify it under
// the terms of the Do What The Fuck You Want To Public License, Version 2, as
// published by Sam Hocevar. See the COPYING file for more details.

use futures::{future, Future};
use std::io::stdin;
use super::Message;
use super::Node;
use super::record::Record;
use super::RFuture;

pub struct StdinReader;

impl StdinReader {
    pub fn new() -> StdinReader {
        StdinReader {}
    }
}

impl Node for StdinReader {
    type Input = ();
    fn process(&mut self, _: Self::Input) -> RFuture {
        let mut buffer = String::new();
        let record = match stdin().read_line(&mut buffer) {
            Ok(s) => {
                if s == 0 {
                    future::ok(Message::Done)
                } else {
                    future::ok(Message::Record(Record {
                        timestamp: Some(::time::now()),
                        raw: buffer.trim().to_string(),
                        ..Default::default()
                    }))
                }
            }
            Err(_) => future::err("Failed to read stdin".into()),
        };
        record.boxed()
    }
}
