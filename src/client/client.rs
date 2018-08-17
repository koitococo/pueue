use byteorder::{BigEndian, WriteBytesExt};
use futures::Future;
use std::io::Error as io_Error;
use tokio::prelude::*;
use tokio_io::io as tokio_io;
use tokio_uds::UnixStream;
use tokio_core::reactor::Handle;

use communication::local::get_unix_stream;
use settings::Settings;
use client::cli::get_app;

/// The client
pub struct Client {
    handle: Handle,
    settings: Settings,
    message: Vec<u8>,
    response: Option<String>,
    communication_future: Option<Box<Future<Item = (UnixStream, Vec<u8>), Error = io_Error> + Send>>,
}

impl Client {
    pub fn new(settings: Settings, handle: Handle, message: Vec<u8>) -> Self {
        Client {
            handle: handle,
            settings: settings,
            message: message,
            response: None,
            communication_future: None,
        }
    }

    pub fn send_message(&mut self) {
        // Early return if we are already waiting for a future.
        if self.communication_future.is_some() {
            return
        }

        // Create a new tokio core
        let unix_stream = get_unix_stream(&self.settings, &self.handle);

        // Get commandline arguments
        let matches = get_app();

        // Get command
        let message = matches.value_of("command").unwrap();
        let command_type = 1 as u64;

        // Prepare command for transfer and determine message byte length
        let payload = message.as_bytes();
        let byte_size = payload.len() as u64;

        let mut header = vec![];
        header.write_u64::<BigEndian>(byte_size).unwrap();
        header.write_u64::<BigEndian>(command_type).unwrap();

        // Send the request size header first.
        // Afterwards send the request.
        let communication_future = tokio_io::write_all(unix_stream, header)
            .and_then(move |(stream, _written)| tokio_io::write_all(stream, payload.clone()))
            .and_then(|(stream, _written)| {
                tokio_io::read_to_end(stream, Vec::new())
            });

        self.communication_future = Some(Box::new(communication_future));
    }

    pub fn receive_answer(&mut self) -> bool {
        // Now receive the response until the connection closes.
        let result = self.communication_future.poll();

        // Handle socket error
        if result.is_err() {
            println!("Socket errored during read");
            println!("{:?}", result.err());

            panic!("Communication failed.");
        }

        // We received a response from the daemon. Handle it
        match result.unwrap() {
            Async::Ready(response_result) => {
                let (_, response_bytes) = if let Some((stream, response_bytes)) = response_result {
                    (stream, response_bytes)
                } else {
                    // Handle socket error
                    println!("Received an empty message from the daemon.");
                    panic!("Communication failed.");
                };

                // Extract response and handle invalid utf8
                let response_result = String::from_utf8(response_bytes);

                let response = if let Ok(response) = response_result {
                    response
                } else {
                    println!("Didn't receive valid utf8.");
                    panic!("Communication failed.");
                };

                self.response = Some(response);

                true
            }
            Async::NotReady => {
                false
            }
        }
    }

    pub fn handle_response(&self) -> bool {
        let response = if let Some(ref response) = self.response {
            response
        } else {
            return false;
        };


        println!("{}", &response);

        return true
    }
}

impl Future for Client {
    type Item = ();
    type Error = String;

    /// The poll function of the daemon.
    /// This is continuously called by the Tokio core.
    fn poll(&mut self) -> Result<Async<()>, Self::Error> {
        // Create the message payload and send it to the daemon.
        self.send_message();

        // Check if we can receive the response from the daemon
        let answer_received = self.receive_answer();

        // Return NotReady until the response has been received and handled.
        if answer_received {
            // Handle the response from the daemon
            self.handle_response();

            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }
}