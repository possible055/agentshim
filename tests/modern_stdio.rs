use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use base64::Engine as _;
use serde_json::{Map, Value, json};

const MAX_RECEIVE_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[path = "modern_stdio/bash.rs"]
mod bash;
#[path = "modern_stdio/capacity.rs"]
mod capacity;
#[path = "modern_stdio/process.rs"]
mod process;
#[path = "modern_stdio/protocol.rs"]
mod protocol;
#[path = "modern_stdio/read_pdf.rs"]
mod read_pdf;
#[path = "modern_stdio/scope.rs"]
mod scope;
#[path = "modern_stdio/support.rs"]
mod support;
