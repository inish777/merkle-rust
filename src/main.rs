// Copyright 2017, Paul Hammant
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate env_logger;
extern crate futures;
extern crate hyper;
#[macro_use]
extern crate log;
extern crate notify;
extern crate sha1;

mod worker;

#[cfg(test)]
mod test;

use notify::{raw_watcher, RawEvent, RecursiveMode, Watcher};
use std::fs::File;
use std::io::BufReader;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use std::ffi::{OsStr, OsString};
use hyper::server::{Http, Request, Response, Service};
use hyper::header::ContentType;
use hyper::StatusCode;
use hyper::mime;
use futures::future;
use futures::future::Future;

struct WebServer;

impl Service for WebServer {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, req: Request) -> Self::Future {
        let data_path_str = format!("data/{}", req.path());
        let data_path = Path::new(data_path_str.as_str());
        if !data_path.exists() {
            return Box::new(future::ok(
                Response::new()
                    .with_status(StatusCode::NotFound)
                    .with_body("not there"),
            ));
        }

        if data_path.is_file() {
            let mut data = String::new();
            let result = File::open(data_path).and_then(|mut f| f.read_to_string(&mut data));
            match result {
                Ok(_) => {
                    return Box::new(future::ok(
                        Response::new()
                            .with_header(ContentType(mime::APPLICATION_JSON))
                            .with_body(data),
                    ));
                }
                Err(_) => {
                    return Box::new(future::ok(Response::new().with_body("nope")));
                }
            }
        } else {
            let mut listdir: Vec<std::fs::DirEntry> = data_path.read_dir().unwrap().map(|r| r.unwrap()).collect();
            listdir.sort_by_key(|dir| dir.path());
            let mut rv: String = get_contents(data_path.join(".sha1").as_ref()) + "\n";
            for entry in listdir {
                if entry.path().to_str().unwrap().ends_with(".sha1") {
                    continue;
                } else {
                    rv += (entry.file_name().to_str().unwrap().to_string() + " ").as_ref();
                    let path: String = entry.path().to_str().unwrap().to_owned() + ".sha1";
                    let path_item = Path::new(&path);
                    if path_item.is_file() {
                        rv += (get_contents(path_item.as_ref()) + "\n").as_str();
                    } else {
                        rv += (get_contents(entry.path().join(".sha1").as_ref()) + "\n").as_str();
                    }
                }
            }
            return Box::new(future::ok(
                Response::new()
                    .with_body(rv)
                    .with_header(ContentType(mime::TEXT_PLAIN)),
            ));
        }
    }
}


pub fn comma_separated_list(dir_sha1s: &Vec<String>) -> String {
    let mut dir_sha1s = dir_sha1s.clone();
    dir_sha1s.sort();
    dir_sha1s.as_slice().join(",")
}

pub fn get_contents(path: &Path) -> String {
    if !path.exists() {
        return "".to_string();
    }
    let file = File::open(path).unwrap();
    let mut reader = BufReader::new(file);
    let mut contents = String::new();
    reader.read_to_string(&mut contents).unwrap();
    contents
}

pub fn write_contents(path: &Path, contents: &str) {
    let mut file = File::create(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

pub fn make_sha1(input: &str) -> String {
    let mut sha1 = sha1::Sha1::new();
    sha1.update(input.as_bytes());
    sha1.digest().to_string()
}

fn process_directory(dir: &Path) -> String {
    let mut dir_sha1s = Vec::new();

    for entry in dir.read_dir().unwrap() {
        let path = entry.unwrap().path();

        if path.is_dir() {
            dir_sha1s.push(process_directory(&path));
        } else if let Some("json") = path.extension().and_then(OsStr::to_str) {
            let sha1_file = format!("{}.sha1", path.to_string_lossy());
            let sha1 = make_sha1(&get_contents(&path));
            if sha1 != get_contents(Path::new(&sha1_file)) {
                write_contents(Path::new(&sha1_file), &sha1);
            }
            dir_sha1s.push(sha1);
        }
    }

    let sha1 = make_sha1(&comma_separated_list(&dir_sha1s));
    let sha1_file = dir.join(".sha1");
    if sha1 != get_contents(&sha1_file) {
        write_contents(&sha1_file, &sha1);
    }

    sha1
}

fn run(path: &str) {
    let start = Instant::now();
    info!("Starting initial sha1 generation");
    let sha1 = process_directory(Path::new(path));
    info!(
        "Initial generation finished - root sha1: {}, duration: {}s",
        sha1,
        start.elapsed().as_secs()
    );

    let jobs = Arc::new(Mutex::new(Vec::new()));

    let path = Path::new(&path).to_path_buf().canonicalize().unwrap();
    let thread_path = path.clone();
    let thread_jobs = jobs.clone();
    thread::spawn(|| worker::work(thread_path, thread_jobs));

    let (tx, rx) = channel();
    let mut watcher = raw_watcher(tx).unwrap();
    watcher.watch(&path, RecursiveMode::Recursive).unwrap();
    thread::spawn(move || {
        let addr = "127.0.0.1:8080".parse().unwrap();
        let server = Http::new().bind(&addr, || Ok(WebServer)).unwrap();
        server.run().unwrap();
    });

    loop {
        match rx.recv() {
            Ok(RawEvent {
                path: Some(path),
                op: Ok(op),
                ..
            }) => {
                debug!("FS event - path: {:?}, op: {:?}", path, op);
                if op & ::notify::op::WRITE == ::notify::op::WRITE {
                    if let Some(extension) = path.clone().extension() {
                        if extension == "json" {
                            info!("Queueing job for {:?}", path);
                            let mut guard = match jobs.lock() {
                                Ok(guard) => guard,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            (*guard).push(path);
                        }
                    }
                }
            }
            Ok(event) => error!("Broken event: {:?}", event),
            Err(e) => error!("Watch error: {:?}", e),
        }
    }
}

fn main() {
    env_logger::init().unwrap();

    if let Some(path) = std::env::args().nth(1) {
        run(&path);
    } else {
        println!("Specify data directory - `cargo run -- <directory>`");
        std::process::exit(1);
    }
}
