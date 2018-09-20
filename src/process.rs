use std::fs::File;
use std::io::Error as IOError;
use std::io::Read;
use std::option::Option;
use std::result::Result;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Duration;

use jsonrpc_core;
use reqwest;
use serde_json;
use serde_json::Value;
use subprocess::{Exec, Popen, PopenError, Redirection};

use super::rpc::types::NodeStatus;

pub enum Error {
    EnvParseError,
    AlreadyRunning,
    NotRunning,
    SubprocessError(PopenError),
    IO(IOError),
    // This error caused when sending HTTP request to the codechain
    CodeChainRPC(String),
}

impl From<PopenError> for Error {
    fn from(error: PopenError) -> Self {
        Error::SubprocessError(error)
    }
}

impl From<IOError> for Error {
    fn from(error: IOError) -> Self {
        Error::IO(error)
    }
}

pub struct ProcessOption {
    pub codechain_dir: String,
    pub log_file_path: String,
}

pub struct Process {
    option: ProcessOption,
    // first element is CodeChain second element is `tee` command
    child: Option<Vec<Popen>>,
}

pub enum Message {
    Run {
        env: String,
        args: String,
        callback: Sender<Result<(), Error>>,
    },
    Stop {
        callback: Sender<Result<(), Error>>,
    },
    Quit {
        callback: Sender<Result<(), Error>>,
    },
    GetStatus {
        callback: Sender<Result<NodeStatus, Error>>,
    },
    GetLog {
        callback: Sender<Result<String, Error>>,
    },
    CallRPC {
        method: String,
        arguments: Vec<Value>,
        callback: Sender<Result<Value, Error>>,
    },
}

impl Process {
    pub fn run_thread(option: ProcessOption) -> Sender<Message> {
        let mut process = Self {
            option,
            child: None,
        };
        let (tx, rx) = channel();
        thread::Builder::new()
            .name("process".to_string())
            .spawn(move || {
                for message in rx {
                    match message {
                        Message::Run {
                            env,
                            args,
                            callback,
                        } => {
                            let result = process.run(env, args);
                            callback.send(result).expect("Callback should be success");
                        }
                        Message::Stop {
                            callback,
                        } => {
                            let result = process.stop();
                            callback.send(result).expect("Callback should be success");
                        }
                        Message::Quit {
                            callback,
                        } => {
                            callback.send(Ok(())).expect("Callback should be success");
                            break
                        }
                        Message::GetStatus {
                            callback,
                        } => {
                            let status = if process.is_running() {
                                NodeStatus::Run
                            } else {
                                NodeStatus::Stop
                            };
                            callback.send(Ok(status)).expect("Callback should be success");
                        }
                        Message::GetLog {
                            callback,
                        } => {
                            let result = process.get_log();
                            callback.send(result).expect("Callback should be success");
                        }
                        Message::CallRPC {
                            method,
                            arguments,
                            callback,
                        } => {
                            let result = process.call_rpc(method, arguments);
                            callback.send(result).expect("Callback should be success")
                        }
                    }
                }
            })
            .expect("Should success running process thread");
        tx
    }

    pub fn run(&mut self, env: String, args: String) -> Result<(), Error> {
        if self.is_running() {
            return Err(Error::AlreadyRunning)
        }

        let args_iter = args.split_whitespace();
        let args_vec: Vec<String> = args_iter.map(|str| str.to_string()).collect();

        let envs = Self::parse_env(&env)?;

        let mut exec = Exec::cmd("cargo")
            .arg("run")
            .arg("--")
            .cwd(self.option.codechain_dir.clone())
            .stdout(Redirection::Pipe)
            .stderr(Redirection::Merge)
            .args(&args_vec);

        for (k, v) in envs {
            exec = exec.env(k, v);
        }

        let child = (exec | Exec::cmd("tee").arg(self.option.log_file_path.clone())).popen()?;
        self.child = Some(child);

        Ok(())
    }

    pub fn is_running(&mut self) -> bool {
        if self.child.is_none() {
            return false
        }

        let child = self.child.as_mut().unwrap();
        if child[0].poll().is_none() {
            return true
        } else {
            return false
        }
    }

    fn parse_env(env: &str) -> Result<Vec<(&str, &str)>, Error> {
        let env_kvs = env.split_whitespace();
        let mut ret = Vec::new();
        for env_kv in env_kvs {
            let kv_array: Vec<&str> = env_kv.split("=").collect();
            if kv_array.len() != 2 {
                return Err(Error::EnvParseError)
            } else {
                ret.push((kv_array[0], kv_array[1]));
            }
        }
        return Ok(ret)
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        if !self.is_running() {
            return Err(Error::NotRunning)
        }

        let codechain = &mut self.child.as_mut().expect("Already checked")[0];
        ctrace!("Send SIGTERM to CodeChain");
        codechain.terminate()?;

        let wait_result = codechain.wait_timeout(Duration::new(10, 0))?;

        if let Some(exit_code) = wait_result {
            ctrace!("CodeChain closed with {:?}", exit_code);
            return Ok(())
        }

        cinfo!("CodeChain does not exit after 10 seconds");

        codechain.kill()?;

        Ok(())
    }

    fn get_log(&mut self) -> Result<String, Error> {
        let file_name = self.option.log_file_path.clone();
        let mut file = File::open(file_name)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    }

    fn call_rpc(&mut self, method: String, arguments: Vec<Value>) -> Result<Value, Error> {
        // FIXME: Get port number from args
        let port = 8080;

        let params = jsonrpc_core::Params::Array(arguments);

        let jsonrpc_request = jsonrpc_core::MethodCall {
            jsonrpc: None,
            method,
            params: Some(params),
            id: jsonrpc_core::Id::Num(1),
        };

        let url = format!("http://127.0.0.1:{}/", port);
        let client = reqwest::Client::new();
        let mut response =
            client.get(&url).json(&jsonrpc_request).send().map_err(|err| Error::CodeChainRPC(format!("{}", err)))?;

        let response: jsonrpc_core::Response =
            response.json().map_err(|err| Error::CodeChainRPC(format!("JSON parse failed {}", err)))?;
        let value = serde_json::to_value(response).expect("Should success jsonrpc type to Value");

        Ok(value)
    }
}
