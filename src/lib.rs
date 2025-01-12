// Copyright (c) 2016-2021 Snowplow Analytics Ltd. All rights reserved.
//
// This program is licensed to you under the Apache License Version 2.0, and
// you may not use this file except in compliance with the Apache License
// Version 2.0.  You may obtain a copy of the Apache License Version 2.0 at
// http://www.apache.org/licenses/LICENSE-2.0.
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the Apache License Version 2.0 is distributed on an "AS
// IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
// implied.  See the Apache License Version 2.0 for the specific language
// governing permissions and limitations there under.
//

#[macro_use]
extern crate log;
extern crate log4rs;
extern crate docopt;
extern crate daggy;
extern crate rustc_serialize;
extern crate valico;
extern crate colored;
extern crate chrono;
extern crate rand;
extern crate crypto;
extern crate uuid;
extern crate hyper;
extern crate hyper_native_tls;
extern crate libc;
extern crate ifaces;
extern crate dns_lookup;

use std::fs;
use factotum::executor::task_list::{Task, State};
use factotum::factfile::Factfile;
use factotum::factfile::Task as FactfileTask;
use factotum::parser::OverrideResultMappings;
use factotum::parser::TaskReturnCodeMapping;
use factotum::executor::execution_strategy::*;
use factotum::webhook::Webhook;
use factotum::executor::ExecutionUpdate;
use factotum::webhook;
use colored::*;
use std::time::Duration;
use std::process::Command;
use std::io::Write;
use std::fs::OpenOptions;
use std::env;
use hyper::Url;
use std::sync::mpsc;
use std::net;
use rustc_serialize::json::{self, Json};
use std::collections::BTreeMap;
#[cfg(test)]
use std::fs::File;
use std::collections::HashMap;
use std::error::Error;

pub mod factotum;

const PROC_SUCCESS: i32 = 0;
const PROC_PARSE_ERROR: i32 = 1;
const PROC_EXEC_ERROR: i32 = 2;
const PROC_OTHER_ERROR: i32 = 3;

// macro to simplify printing to stderr
// https://github.com/rust-lang/rfcs/issues/1078
macro_rules! print_err {
    ($($arg:tt)*) => (
        {
            use std::io::prelude::*;
            if let Err(e) = write!(&mut ::std::io::stderr(), "{}\n", format_args!($($arg)*)) {
                panic!("Failed to write to stderr.\
                    \nOriginal error output: {}\
                    \nSecondary error writing to stderr: {}", format!($($arg)*), e);
            }
        }
    )
}

fn get_duration_as_string(d: &Duration) -> String {
    // duration doesn't support the normal display format
    // for now lets put together something that produces some sensible output
    // e.g.
    // if it's under a minute, show the number of seconds and nanos
    // if it's under an hour, show the number of minutes, and seconds
    // if it's over an hour, show the number of hours, minutes and seconds
    const NANOS_ONE_SEC: f64 = 1000000000_f64;
    const SECONDS_ONE_HOUR: u64 = 3600;

    if d.as_secs() < 60 {
        let mut seconds: f64 = d.as_secs() as f64;
        seconds += d.subsec_nanos() as f64 / NANOS_ONE_SEC;
        format!("{:.1}s", seconds)
    } else if d.as_secs() >= 60 && d.as_secs() < SECONDS_ONE_HOUR {
        // ignore nanos here..
        let secs = d.as_secs() % 60;
        let minutes = (d.as_secs() / 60) % 60;
        format!("{}m, {}s", minutes, secs)
    } else {
        let secs = d.as_secs() % 60;
        let minutes = (d.as_secs() / 60) % 60;
        let hours = d.as_secs() / SECONDS_ONE_HOUR;
        format!("{}h, {}m, {}s", hours, minutes, secs)
    }
}

fn get_task_result_line_str(task_result: &Task<&FactfileTask>) -> (String, Option<String>) {

    let state = task_result.state.clone();
    let start_time = match task_result.run_started {
        Some(ref t) => Some(format!("{}", t)),
        _ => None,
    };
    let (opening_line, stdout, stderr, summary_line) = if let Some(ref res) =
        task_result.run_result {
        // we know tasks with run details were attempted

        let opener = format!("Task '{}' was started at {}\n",
                             task_result.name.cyan(),
                             start_time.unwrap());

        let output = match res.stdout {
            Some(ref o) => {
                Some(format!("Task '{}' stdout:\n{}\n",
                             task_result.name.cyan(),
                             o.trim_right().bold()))
            } 
            None => None,
        };

        let errors = match res.stderr {
            Some(ref e) => {
                Some(format!("Task '{}' stderr:\n{}\n",
                             task_result.name.cyan(),
                             e.trim_right().red()))
            }
            None => None,
        };

        let summary = match (&res.task_execution_error, state) {
            (&Some(ref task_exec_error_msg), _) => {
                let mut failure_str = "Task '".red().to_string();
                failure_str.push_str(&format!("{}", task_result.name.cyan()));
                failure_str.push_str(&format!("': couldn't be started. Reason: {}", task_exec_error_msg).red().to_string());
                failure_str
            }
            (_, State::Failed(fail_reason)) => {
                let mut failure_str = "Task '".red().to_string();
                failure_str.push_str(&format!("{}", task_result.name.cyan()));
                failure_str.push_str(&format!("': failed after {}. Reason: {}",
                                              get_duration_as_string(&res.duration),
                                              fail_reason)
                    .red()
                    .to_string());
                failure_str
            }
            (_, _) => {
                let mut success_str = "Task '".green().to_string();
                success_str.push_str(&format!("{}", task_result.name.cyan()));
                success_str.push_str(&format!("': succeeded after {}",
                                              get_duration_as_string(&res.duration))
                    .green()
                    .to_string());
                success_str
            }
        };

        (opener, output, errors, summary)

    } else {
        // tasks without run details may have been unable to start (some internal error)
        // or skipped because a prior task errored or NOOPed

        let reason_for_not_running = if let State::Failed(_) = task_result.state {
            "Factotum could not start the task".red().to_string()
        } else {
            "skipped".to_string()
        };

        let opener = format!("Task '{}': {}!\n",
                             task_result.name.cyan(),
                             reason_for_not_running);
        (opener, None, None, String::from(""))
    };

    let mut result = opening_line;
    if let Some(o) = stdout {
        result.push_str(&o);
    }

    if summary_line.len() > 0 {
        result.push_str(&format!("{}\n", summary_line));
    }

    return (result, stderr);
}

fn get_task_results_str(task_results: &Vec<&Task<&FactfileTask>>) -> (String, String) {
    let mut stderr = String::new();
    let mut stdout = String::new();

    let mut total_run_time = Duration::new(0, 0);
    let mut executed = 0;

    for task in task_results.iter() {
        let (task_stdout, task_stderr) = get_task_result_line_str(task);
        stdout.push_str(&task_stdout);

        if let Some(task_stderr_str) = task_stderr {
            stderr.push_str(&task_stderr_str);
        }

        if let Some(ref run_result) = task.run_result {
            total_run_time = total_run_time + run_result.duration;
            executed += 1;
        }
    }

    let summary = format!("{}/{} tasks run in {}\n",
                          executed,
                          task_results.len(),
                          get_duration_as_string(&total_run_time));
    stdout.push_str(&summary.green().to_string());

    (stdout, stderr)
}

pub fn validate_start_task(job: &Factfile, start_task: &str) -> Result<(), &'static str> {
    // A
    // / \
    // B   C
    // / \ /
    // D   E
    //
    // We cannot start at B because E depends on C, which depends on A (simiar for C)
    //
    // A
    // / \
    // B   C
    // / \   \
    // D   E   F
    //
    // It's fine to start at B here though, causing B, D, and E to be run
    //


    match job.can_job_run_from_task(start_task) {
        Ok(is_good) => {
            if is_good {
                Ok(())
            } else {
                Err("the job cannot be started here without triggering prior tasks")
            }
        }
        Err(msg) => Err(msg),
    }
}

fn dot(factfile: &str, start_from: Option<String>) -> Result<String, String> {
    let ff = try!(factotum::parser::parse(factfile, None, OverrideResultMappings::None));
    if let Some(ref start) = start_from {
        match ff.can_job_run_from_task(&start) {
            Ok(is_good) => {
                if !is_good {
                    return Err("the job cannot be started here.".to_string());
                }
            }
            Err(msg) => return Err(msg.to_string()),
        }
    }

    Ok(ff.as_dotfile(start_from))
}

fn validate(factfile: &str, env: Option<Json>) -> Result<String, String> {
    match factotum::parser::parse(factfile, env, OverrideResultMappings::None) {
        Ok(_) => Ok(format!("'{}' is a valid Factfile!", factfile).green().to_string()),
        Err(msg) => Err(msg.red().to_string()),
    }
}

fn parse_file_and_simulate(factfile: &str, env: Option<Json>, start_from: Option<String>) -> i32 {
    parse_file_and_execute_with_strategy(factfile,
                                         env,
                                         start_from,
                                         factotum::executor::execution_strategy::execute_simulation,
                                         OverrideResultMappings::All(TaskReturnCodeMapping {
                                             continue_job: vec![0],
                                             terminate_early: vec![],
                                         }),
                                         None,
                                         None,
                                         None)
}

fn parse_file_and_execute(factfile: &str,
                          env: Option<Json>,
                          start_from: Option<String>,
                          webhook_url: Option<String>,
                          job_tags: Option<HashMap<String, String>>,
                          max_stdouterr_size: Option<usize>)
                          -> i32 {
    parse_file_and_execute_with_strategy(factfile,
                                         env,
                                         start_from,
                                         factotum::executor::execution_strategy::execute_os,
                                         OverrideResultMappings::None,
                                         webhook_url,
                                         job_tags,
                                         max_stdouterr_size)
}

fn parse_file_and_execute_with_strategy<F>(factfile: &str,
                                           env: Option<Json>,
                                           start_from: Option<String>,
                                           strategy: F,
                                           override_result_map: OverrideResultMappings,
                                           webhook_url: Option<String>,
                                           job_tags: Option<HashMap<String, String>>,
                                           max_stdouterr_size: Option<usize>)
                                           -> i32
    where F: Fn(&str, &mut Command) -> RunResult + Send + Sync + 'static + Copy
{

    match factotum::parser::parse(factfile, env, override_result_map) {
        Ok(job) => {

            if let Some(ref start_task) = start_from {
                if let Err(msg) = validate_start_task(&job, &start_task) {
                    warn!("The job could not be started from '{}' because {}",
                          start_task,
                          msg);
                    println!("The job cannot be started from '{}' because {}",
                             start_task.cyan(),
                             msg);
                    return PROC_OTHER_ERROR;
                }
            }

            let (maybe_updates_channel, maybe_join_handle) = if webhook_url.is_some() {
                let url = webhook_url.unwrap();
                let mut wh = Webhook::new(job.name.clone(), job.raw.clone(), url, job_tags, max_stdouterr_size);
                let (tx, rx) = mpsc::channel::<ExecutionUpdate>();
                let join_handle =
                    wh.connect_webhook(rx, Webhook::http_post, webhook::backoff_rand_1_minute);
                (Some(tx), Some(join_handle))
            } else {
                (None, None)
            };

            let job_res = factotum::executor::execute_factfile(&job,
                                                               start_from,
                                                               strategy,
                                                               maybe_updates_channel);

            let mut has_errors = false;
            let mut has_early_finish = false;

            let mut tasks = vec![];

            for task_group in job_res.tasks.iter() {
                for task in task_group {
                    if let State::Failed(_) = task.state {
                        has_errors = true;
                    } else if let State::SuccessNoop = task.state {
                        has_early_finish = true;
                    }
                    tasks.push(task);
                }
            }

            let normal_completion = !has_errors && !has_early_finish;

            let result = if normal_completion {
                let (stdout_summary, stderr_summary) = get_task_results_str(&tasks);
                print!("{}", stdout_summary);
                if !stderr_summary.trim_right().is_empty() {
                    print_err!("{}", stderr_summary.trim_right());
                }
                PROC_SUCCESS
            } else if has_early_finish && !has_errors {
                let (stdout_summary, stderr_summary) = get_task_results_str(&tasks);
                print!("{}", stdout_summary);
                if !stderr_summary.trim_right().is_empty() {
                    print_err!("{}", stderr_summary.trim_right());
                }
                let incomplete_tasks = tasks.iter()
                    .filter(|r| !r.run_result.is_some())
                    .map(|r| format!("'{}'", r.name.cyan()))
                    .collect::<Vec<String>>()
                    .join(", ");
                let stop_requesters = tasks.iter()
                    .filter(|r| match r.state {
                        State::SuccessNoop => true,
                        _ => false,
                    })
                    .map(|r| format!("'{}'", r.name.cyan()))
                    .collect::<Vec<String>>()
                    .join(", ");
                println!("Factotum job finished early as a task ({}) requested an early finish. \
                          The following tasks were not run: {}.",
                         stop_requesters,
                         incomplete_tasks);
                PROC_SUCCESS
            } else {
                let (stdout_summary, stderr_summary) = get_task_results_str(&tasks);
                print!("{}", stdout_summary);

                if !stderr_summary.trim_right().is_empty() {
                    print_err!("{}", stderr_summary.trim_right());
                }

                let incomplete_tasks = tasks.iter()
                    .filter(|r| !r.run_result.is_some())
                    .map(|r| format!("'{}'", r.name.cyan()))
                    .collect::<Vec<String>>()
                    .join(", ");

                let failed_tasks = tasks.iter()
                    .filter(|r| match r.state {
                        State::Failed(_) => true,
                        _ => false,
                    })
                    .map(|r| format!("'{}'", r.name.cyan()))
                    .collect::<Vec<String>>()
                    .join(", ");

                println!("Factotum job executed abnormally as a task ({}) failed - the following \
                          tasks were not run: {}!",
                         failed_tasks,
                         incomplete_tasks);
                PROC_EXEC_ERROR
            };

            if maybe_join_handle.is_some() {
                print!("Waiting for webhook to finish sending events...");
                let j = maybe_join_handle.unwrap();
                let webhook_res = j.join().ok().unwrap();
                println!("{}", " done!".green());

                if webhook_res.events_received > webhook_res.success_count {
                    println!("{}", "Warning: some events failed to send".red());
                }
            }

            result
        } 
        Err(msg) => {
            println!("{}", msg);
            return PROC_PARSE_ERROR;
        }      
    }
}

fn write_to_file(filename: &str, contents: &str, overwrite: bool) -> Result<(), String> {
    let mut f = if overwrite {
        match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(filename) {
            Ok(f) => f,
            Err(io) => return Err(format!("couldn't create file '{}' ({})", filename, io)),        
        }
    } else {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(filename) {
            Ok(f) => f,
            Err(io) => return Err(format!("couldn't create file '{}' ({})", filename, io)),        
        }
    };

    match f.write_all(contents.as_bytes()) {
        Ok(_) => Ok(()),
        Err(msg) => Err(format!("couldn't write to file '{}' ({})", filename, msg)),
    }
}

pub fn is_valid_url(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        match Url::parse(url) {
            Ok(_) => Ok(()),
            Err(msg) => Err(format!("{}", msg)),
        }
    } else {
        Err("URL must begin with 'http://' or 'https://'.".into())
    }
}

fn get_constraint_map(constraints: &Vec<String>) -> HashMap<String, String> {
    get_tag_map(constraints)
}

fn is_valid_host(host: &str) -> Result<(), String> {
    if host == "*" {
        return Ok(());
    }

    let os_hostname = try!(gethostname_safe().map_err(|e| e.to_string()));

    if host == os_hostname {
        return Ok(());
    }

    let external_addrs = try!(get_external_addrs().map_err(|e| e.to_string()));
    let host_addrs = try!(dns_lookup::lookup_host(&host)
        .map_err(|_| "could not find any IPv4 addresses for the supplied hostname"));

    for host_addr in host_addrs {
        if let Ok(good_host_addr) = host_addr {
            if external_addrs.iter().any(|external_addr| external_addr.ip() == good_host_addr) {
                return Ok(());
            }
        }
    }

    Err("failed to match any of the interface addresses to the found host addresses".into())
}

extern "C" {
    pub fn gethostname(name: *mut libc::c_char, size: libc::size_t) -> libc::c_int;
}

fn gethostname_safe() -> Result<String, String> {
    let len = 255;
    let mut buf = Vec::<u8>::with_capacity(len);

    let ptr = buf.as_mut_slice().as_mut_ptr();

    let err = unsafe { gethostname(ptr as *mut libc::c_char, len as libc::size_t) } as libc::c_int;

    match err {
        0 => {
            let mut _real_len = len;
            let mut i = 0;
            loop {
                let byte = unsafe { *(((ptr as u64) + (i as u64)) as *const u8) };
                if byte == 0 {
                    _real_len = i;
                    break;
                }
                i += 1;
            }
            unsafe { buf.set_len(_real_len) }
            Ok(String::from_utf8_lossy(buf.as_slice()).into_owned())
        }
        _ => {
            Err("could not get hostname from system; cannot compare against supplied hostname"
                .into())
        }
    }
}

fn get_external_addrs() -> Result<Vec<net::SocketAddr>, String> {
    let mut external_addrs = vec![];

    for iface in ifaces::Interface::get_all().unwrap().into_iter() {
        if iface.kind == ifaces::Kind::Ipv4 {
            if let Some(addr) = iface.addr {
                if !addr.ip().is_loopback() {
                    external_addrs.push(addr)
                }
            }
        }
    }

    if external_addrs.len() == 0 {
        Err("could not find any non-loopback IPv4 addresses in the network interfaces; do you \
             have a working network interface card?"
            .into())
    } else {
        Ok(external_addrs)
    }
}

fn get_tag_map(args: &Vec<String>) -> HashMap<String, String> {
    let mut arg_map: HashMap<String, String> = HashMap::new();

    for arg in args.iter() {
        let split = arg.split(",").collect::<Vec<&str>>();
        if split.len() >= 2 && split[0].trim().is_empty() == false {
            let key = split[0].trim().to_string();
            let value = split[1..].join("").trim().to_string();
            arg_map.insert(key, value);
        } else if split.len() == 1 && split[0].trim().is_empty() == false {
            let key = split[0].trim().to_string();
            let value = "".to_string();
            arg_map.insert(key, value);
        }
    }

    arg_map
}

#[test]
fn test_tag_map() {
    let easy = get_tag_map(&vec!["hello,world".to_string()]);
    let mut expected_easy = HashMap::new();
    expected_easy.insert("hello".to_string(), "world".to_string());
    assert_eq!(easy, expected_easy);

    let trim_leading_trailing = get_tag_map(&vec!["  hello   ,  world   ".to_string()]);
    assert_eq!(trim_leading_trailing, expected_easy);

    let missing_value = get_tag_map(&vec!["  hello   ".to_string()]);
    let mut expected_missing_value = HashMap::new();
    expected_missing_value.insert("hello".to_string(), "".to_string());
    assert_eq!(missing_value, expected_missing_value);

    let empty = get_tag_map(&vec![" ".to_string()]);
    assert_eq!(empty, HashMap::new());

    let empty_key = get_tag_map(&vec![" , asdas".to_string()]);
    assert_eq!(empty_key, HashMap::new());

    let with_comma = get_tag_map(&vec!["the rain,first,, wow,,".to_string()]);
    let mut expected_comma = HashMap::new();
    expected_comma.insert("the rain".to_string(), "first wow".to_string());
    assert_eq!(with_comma, expected_comma);
}

fn json_str_to_btreemap(j: &str) -> Result<BTreeMap<String, String>, String> {
    json::decode(j).map_err(|err| {
        format!("Supplied string '{}' is not valid JSON: {}",
                j,
                Error::description(&err))
    })
}

fn str_to_json(s: &str) -> Result<Json, String> {
    Json::from_str(s).map_err(|err| {
        format!("Supplied string '{}' is not valid JSON: {}",
                s,
                Error::description(&err))
    })
}

#[test]
fn str_to_json_produces_json() {
    let sample = "{\"hello\":\"world\"}";
    if let Ok(j) = str_to_json(sample) {
        assert_eq!(Json::from_str(sample).unwrap(), j)
    } else {
        panic!("valid json did not produce inflated json")
    }
}

#[test]
fn str_to_json_bad_json() {
    let invalid = "{\"hello\":\"world\""; // missing final }
    if let Err(msg) = str_to_json(invalid) {
        assert_eq!("Supplied string '{\"hello\":\"world\"' is not valid JSON: failed \
                    to parse json",
                   msg)
    } else {
        panic!("invalid json parsed successfully")
    }
}

fn get_log_config() -> Result<log4rs::config::Config, String> {    
    let file_appender = match log4rs::appender::FileAppender::builder(".factotum/factotum.log").build() {
        Ok(fa) => fa,
        Err(e) => {
            let cwd = env::current_dir().expect("Unable to get current working directory");
            let expanded_path = format!("{}{}{}", cwd.display(), std::path::MAIN_SEPARATOR, ".factotum/factotum.log");
            return Err(format!("couldn't create logfile appender to '{}'. Reason: {}", expanded_path, e.description()));
        }
    };

    let root = log4rs::config::Root::builder(log::LogLevelFilter::Info)
        .appender("file".to_string());

    log4rs::config::Config::builder(root.build())
        .appender(log4rs::config::Appender::builder("file".to_string(),
                                                    Box::new(file_appender)).build())
        .build().map_err(|e| format!("error setting logging. Reason: {}", e.description()))
}

fn init_logger() -> Result<(), String> {
    match fs::create_dir(".factotum") {
        Ok(_) => (),
        Err(e) => match e.kind() {
            std::io::ErrorKind::AlreadyExists => (),
            _ => {
                let cwd = env::current_dir().expect("Unable to get current working directory");
                let expected_path =  format!("{}{}{}{}", cwd.display(), std::path::MAIN_SEPARATOR, ".factotum", std::path::MAIN_SEPARATOR);
                return Err(format!("unable to create directory '{}' for logfile. Reason: {}", expected_path, e.description()))
            }
        }
    };
    let log_config = try!(get_log_config());
    log4rs::init_config(log_config).map_err(|e| format!("couldn't initialize log configuration. Reason: {}", e.description()))
}

pub fn execute_dag(factfile: &str, webhook_url: Option<String>) -> i32 {
    parse_file_and_execute(factfile,
        None,
        None,
        webhook_url,
        None,
        None)
}

#[test]
fn test_is_valid_url() {
    match is_valid_url("http://") {
        Ok(_) => panic!("http:// is not a valid url"),
        Err(msg) => assert_eq!(msg, "empty host"),
    }

    match is_valid_url("http://potato.com/") {
        Ok(_) => (),
        Err(_) => panic!("http://potato.com/ is a valid url"),
    }

    match is_valid_url("https://potato.com/") {
        Ok(_) => (),
        Err(_) => panic!("https://potato.com/ is a valid url"),
    }

    match is_valid_url("potato.com/") {
        Ok(_) => panic!("no http/s?"),
        Err(msg) => {
            assert_eq!(msg,
                       "URL must begin with 'http://' or 'https://' to be used with Factotum \
                        webhooks")
        } // this is good
    }
}

#[test]
fn test_write_to_file() {
    use std::env;
    use std::io::Read;

    let test_file = "factotum-write-test.txt";
    let mut dir = env::temp_dir();
    dir.push(test_file);

    let test_path = &str::replace(&format!("{:?}", dir.as_os_str()), "\"", "");
    println!("test file path: {}", test_path);

    fs::remove_file(test_path).ok();

    assert!(match write_to_file(test_path, "helloworld", false) {
        Ok(_) => true,
        Err(msg) => panic!("Unexpected error: {}", msg),
    });
    assert!(write_to_file(test_path, "helloworld", false).is_err());
    assert!(write_to_file(test_path, "helloworld all", true).is_ok());

    let mut file = File::open(test_path).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();

    assert_eq!(contents, "helloworld all");

    assert!(fs::remove_file(test_path).is_ok());

    // check that overwrite will also write a new file (https://github.com/snowplow/factotum/issues/97)

    assert!(write_to_file(test_path, "overwrite test", true).is_ok());
    assert!(fs::remove_file(test_path).is_ok());
}

#[test]
fn validate_ok_factfile_good() {
    let test_file_path = "./tests/resources/example_ok.factfile";
    let is_valid = validate(test_file_path, None);
    let expected: String = format!("'{}' is a valid Factfile!", test_file_path).green().to_string();
    assert_eq!(is_valid, Ok(expected));
}

#[test]
fn validate_ok_factfile_bad() {
    let test_file_path = "./tests/resources/invalid_json.factfile";
    let is_valid = validate(test_file_path, None);
    match is_valid {
        Ok(_) => panic!("Validation returning valid for invalid file"),
        Err(msg) => {
            let expected = format!("'{}' is not a valid", test_file_path);
            assert!(msg.contains(&expected))
        }
    }
}

#[test]
fn have_valid_config() {
    fs::create_dir(".factotum").ok();
    if let Err(errs) = get_log_config() {
        panic!("config not building correctly! {:?}", errs);
    }
}

#[test]
fn get_duration_under_minute() {
    assert_eq!(get_duration_as_string(&Duration::new(2, 500000099)),
               "2.5s".to_string());
    assert_eq!(get_duration_as_string(&Duration::new(0, 0)),
               "0.0s".to_string());
}

#[test]
fn get_duration_under_hour() {
    assert_eq!(get_duration_as_string(&Duration::new(62, 500000099)),
               "1m, 2s".to_string()); // drop nanos for minute level precision
    assert_eq!(get_duration_as_string(&Duration::new(59 * 60 + 59, 0)),
               "59m, 59s".to_string());
}

#[test]
fn get_duration_with_hours() {
    assert_eq!(get_duration_as_string(&Duration::new(3600, 0)),
               "1h, 0m, 0s".to_string());
    assert_eq!(get_duration_as_string(&Duration::new(3600 * 10 + 63, 0)),
               "10h, 1m, 3s".to_string());
}

#[test]
fn test_get_task_result_line_str() {
    use chrono::UTC;
    use factotum::executor::execution_strategy::RunResult;
    use factotum::factfile::{Task as FactfileTask, OnResult};

    // successful after 20 secs
    let dt = UTC::now();
    let sample_task = Task::<&FactfileTask> {
        name: String::from("hello world"),
        // children: vec![],
        state: State::Success,
        run_started: Some(dt),
        task_spec: &FactfileTask {
            name: "hello world".to_string(),
            depends_on: vec![],
            executor: "".to_string(),
            command: "".to_string(),
            arguments: vec![],
            on_result: OnResult {
                terminate_job: vec![],
                continue_job: vec![],
            },
        },
        run_result: Some(RunResult {
            duration: Duration::from_secs(20),
            task_execution_error: None,
            stdout: Some(String::from("hello world")),
            stderr: None,
            return_code: 0,
        }),
    };

    let expected = format!("Task '{}' was started at {}\nTask '{}' stdout:\n{}\n{}{}{}\n",
                           "hello world".cyan(),
                           dt,
                           "hello world".cyan(),
                           "hello world".bold(),
                           "Task '".green(),
                           "hello world".cyan(),
                           "': succeeded after 20.0s".green());
    let (result_stdout, result_stderr) = get_task_result_line_str(&sample_task);
    assert_eq!(result_stdout, expected);
    assert_eq!(result_stderr, None);

    // failed after 20 secs
    // (was started ok)
    let sample_task_stdout = Task::<&FactfileTask> {
        name: String::from("hello world"),
        // children: vec![],
        state: State::Failed("Something about not being in continue job".to_string()),
        run_started: Some(dt),
        task_spec: &FactfileTask {
            name: "hello world".to_string(),
            depends_on: vec![],
            executor: "".to_string(),
            command: "".to_string(),
            arguments: vec![],
            on_result: OnResult {
                terminate_job: vec![],
                continue_job: vec![],
            },
        },
        run_result: Some(RunResult {
            duration: Duration::from_secs(20),
            task_execution_error: None,
            stdout: Some(String::from("hello world")),
            stderr: Some(String::from("There's errors")),
            return_code: 0,
        }),
    };

    assert_eq!(format!("Task '{}' stderr:\n{}\n",
                       sample_task.name.cyan(),
                       "There's errors".red()),
               get_task_result_line_str(&sample_task_stdout).1.unwrap());
    assert_eq!(get_task_result_line_str(&sample_task_stdout).0,
               format!("Task '{}' was started at {}\nTask '{}' stdout:\n{}\n{}{}{}\n",
                       "hello world".cyan(),
                       dt,
                       "hello world".cyan(),
                       "hello world".bold(),
                       "Task '".red(),
                       "hello world".cyan(),
                       "': failed after 20.0s. Reason: Something about not being in continue job"
                           .red()));

    // skipped task (previous failure/noop)
    let task_skipped = Task::<&FactfileTask> {
        name: String::from("skip"),
        // children: vec![],
        run_started: None,
        task_spec: &FactfileTask {
            name: "hello world".to_string(),
            depends_on: vec![],
            executor: "".to_string(),
            command: "".to_string(),
            arguments: vec![],
            on_result: OnResult {
                terminate_job: vec![],
                continue_job: vec![],
            },
        },
        state: State::Skipped("for some reason".to_string()),
        run_result: None,
    };

    assert_eq!(format!("Task '{}': skipped!\n", "skip".cyan()),
               get_task_result_line_str(&task_skipped).0);
    assert_eq!(None, get_task_result_line_str(&task_skipped).1);

    let task_init_fail = Task::<&FactfileTask> {
        name: String::from("init fail"),
        //  children: vec![],
        state: State::Failed("bla".to_string()),
        run_started: None,
        task_spec: &FactfileTask {
            name: "hello world".to_string(),
            depends_on: vec![],
            executor: "".to_string(),
            command: "".to_string(),
            arguments: vec![],
            on_result: OnResult {
                terminate_job: vec![],
                continue_job: vec![],
            },
        },
        run_result: None,
    };

    assert_eq!(format!("Task '{}': {}!\n",
                       "init fail".cyan(),
                       "Factotum could not start the task".red()),
               get_task_result_line_str(&task_init_fail).0);
    assert_eq!(None, get_task_result_line_str(&task_init_fail).1);

    let task_failure = Task::<&FactfileTask> {
        name: String::from("fails"),
        // children: vec![],
        state: State::Failed("bla".to_string()),
        run_started: Some(dt),
        task_spec: &FactfileTask {
            name: "hello world".to_string(),
            depends_on: vec![],
            executor: "".to_string(),
            command: "".to_string(),
            arguments: vec![],
            on_result: OnResult {
                terminate_job: vec![],
                continue_job: vec![],
            },
        },
        run_result: Some(RunResult {
            duration: Duration::from_secs(20),
            task_execution_error: Some(String::from("The task exited with something unexpected")),
            stdout: Some(String::from("hello world")),
            stderr: Some(String::from("There's errors")),
            return_code: 0,
        }),
    };

    let expected_failed =
        format!("Task '{}' was started at {}\nTask '{}' stdout:\n{}\n{}{}{}\n",
                "fails".cyan(),
                dt,
                "fails".cyan(),
                "hello world".bold(),
                "Task '".red(),
                "fails".cyan(),
                "': couldn't be started. Reason: The task exited with something unexpected".red());
    let (stdout_failed, stderr_failed) = get_task_result_line_str(&task_failure);
    assert_eq!(expected_failed, stdout_failed);
    assert_eq!(format!("Task '{}' stderr:\n{}\n",
                       "fails".cyan(),
                       "There's errors".red()),
               stderr_failed.unwrap());

}

#[test]
fn test_get_task_results_str_summary() {
    use chrono::UTC;
    use factotum::executor::execution_strategy::RunResult;
    use factotum::factfile::{Task as FactfileTask, OnResult};

    let dt = UTC::now();

    let task_one_spec = FactfileTask {
        name: "hello world".to_string(),
        depends_on: vec![],
        executor: "".to_string(),
        command: "".to_string(),
        arguments: vec![],
        on_result: OnResult {
            terminate_job: vec![],
            continue_job: vec![],
        },
    };

    let task_one = Task::<&FactfileTask> {
        name: String::from("hello world"),
        // children: vec![],
        state: State::Success,
        task_spec: &task_one_spec,
        run_started: Some(dt),
        run_result: Some(RunResult {
            duration: Duration::from_secs(20),
            task_execution_error: None,
            stdout: Some(String::from("hello world")),
            stderr: Some(String::from("Mistake")),
            return_code: 0,
        }),
    };


    let task_two_spec = FactfileTask {
        name: "hello world 2".to_string(),
        depends_on: vec![],
        executor: "".to_string(),
        command: "".to_string(),
        arguments: vec![],
        on_result: OnResult {
            terminate_job: vec![],
            continue_job: vec![],
        },
    };

    let task_two = Task::<&FactfileTask> {
        name: String::from("hello world 2"),
        // children: vec![],
        state: State::Success,
        task_spec: &task_two_spec,
        run_started: Some(dt),
        run_result: Some(RunResult {
            duration: Duration::from_secs(80),
            task_execution_error: None,
            stdout: Some(String::from("hello world")),
            stderr: Some(String::from("Mistake")),
            return_code: 0,
        }),
    };

    let mut tasks: Vec<&Task<&FactfileTask>> = vec![];
    let (stdout, stderr) = get_task_results_str(&tasks);
    let expected: String = format!("{}", "0/0 tasks run in 0.0s\n".green());

    assert_eq!(stdout, expected);
    assert_eq!(stderr, "");

    tasks.push(&task_one);

    let (one_task_stdout, one_task_stderr) = get_task_results_str(&tasks);
    let (first_task_stdout, first_task_stderr) = get_task_result_line_str(&tasks[0]);
    let expected_one_task = format!("{}{}",
                                    first_task_stdout,
                                    "1/1 tasks run in 20.0s\n".green());

    assert_eq!(one_task_stdout, expected_one_task);
    let first_task_stderr_str = first_task_stderr.unwrap();
    assert_eq!(one_task_stderr, first_task_stderr_str);

    tasks.push(&task_two);

    let (two_task_stdout, two_task_stderr) = get_task_results_str(&tasks);
    let (task_two_stdout, task_two_stderr) = get_task_result_line_str(&tasks[1]);
    let expected_two_task = format!("{}{}{}",
                                    first_task_stdout,
                                    task_two_stdout,
                                    "2/2 tasks run in 1m, 40s\n".green());
    assert_eq!(two_task_stdout, expected_two_task);
    assert_eq!(two_task_stderr,
               format!("{}{}", first_task_stderr_str, task_two_stderr.unwrap()));

}

#[test]
fn test_start_task_validation_not_present() {
    let mut factfile = Factfile::new("N/A", "test");

    match validate_start_task(&factfile, "something") {
        Err(r) => assert_eq!(r, "the task specified could not be found"),
        _ => unreachable!("validation did not fail"),
    }

    factfile.add_task("something", &vec![], "", "", &vec![], &vec![], &vec![]);
    if let Err(_) = validate_start_task(&factfile, "something") {
        unreachable!("validation failed when task present")
    }
}

#[test]
fn test_start_task_cycles() {

    use factotum::factfile::*;

    let mut factfile = Factfile::new("N/A", "test");

    let task_a = Task {
        name: "a".to_string(),
        depends_on: vec![],
        executor: "".to_string(),
        command: "".to_string(),
        arguments: vec![],
        on_result: OnResult {
            terminate_job: vec![],
            continue_job: vec![],
        },
    };

    let task_b = Task {
        name: "b".to_string(),
        depends_on: vec!["a".to_string()],
        executor: "".to_string(),
        command: "".to_string(),
        arguments: vec![],
        on_result: OnResult {
            terminate_job: vec![],
            continue_job: vec![],
        },
    };

    let task_c = Task {
        name: "c".to_string(),
        depends_on: vec!["a".to_string()],
        executor: "".to_string(),
        command: "".to_string(),
        arguments: vec![],
        on_result: OnResult {
            terminate_job: vec![],
            continue_job: vec![],
        },
    };

    let task_d = Task {
        name: "d".to_string(),
        depends_on: vec!["c".to_string(), "b".to_string()],
        executor: "".to_string(),
        command: "".to_string(),
        arguments: vec![],
        on_result: OnResult {
            terminate_job: vec![],
            continue_job: vec![],
        },
    };

    factfile.add_task_obj(&task_a);
    factfile.add_task_obj(&task_b);
    factfile.add_task_obj(&task_c);
    factfile.add_task_obj(&task_d);

    match validate_start_task(&factfile, "c") {
        Err(r) => {
            assert_eq!(r,
                       "the job cannot be started here without triggering prior tasks")
        }
        _ => unreachable!("the task validated when it shouldn't have"),
    }
}

#[test]
fn test_gethostname_safe() {
    let hostname = gethostname_safe();
    if let Ok(ok_hostname) = hostname {
        assert!(!ok_hostname.is_empty());
    } else {
        panic!("gethostname_safe() must return a Ok(<String>)");
    }
}

#[test]
fn test_get_external_addrs() {
    let external_addrs = get_external_addrs();
    if let Ok(ok_external_addrs) = external_addrs {
        assert!(ok_external_addrs.len() > 0);
    } else {
        panic!("get_external_addrs() must return a Ok(Vec<net::SocketAddr>) that is non-empty");
    }
}

#[test]
fn test_is_valid_host() {
    is_valid_host("*").expect("must be Ok() for wildcard");

    // Test each external addr is_valid_host
    let external_addrs = get_external_addrs()
        .expect("get_external_addrs() must return a Ok(Vec<net::SocketAddr>) that is non-empty");
    for external_addr in external_addrs {
        let ip_str = external_addr.ip().to_string();
        is_valid_host(&ip_str).expect(&format!("must be Ok() for IP {}", &ip_str));
    }
}
