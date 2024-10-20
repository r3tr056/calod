extern crate core;

#[allow(unused_imports)]
use std::env;

#[allow(unused_imports)]
use std::fs;

#[allow(unused_imports)]
use std::net::{TcpListener, TcpStream}
use std::thread;

use calod::handle_connection;
use calod::store::calod_store::{CalodStore, Store}

fn main() {
    println!("Logs from your program will appear here!");

    let listener = TcpListener::bind("127.0.0.1:8857").unwrap()
    CalodStore::initialize();

    for wrapped_stream in listener.incoming() {
        let stream = wrapped_stream.unwrap();
        thread::spawn(move || handle_connection(stream));
    }
}