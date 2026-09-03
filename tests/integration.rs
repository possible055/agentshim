use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, Write},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use base64::Engine as _;
use chrono::{Days, Utc};
use serde_json::{Value, json};

#[path = "common/mod.rs"]
mod common;

const MAX_RECEIVE_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[path = "integration/bash.rs"]
mod bash;
#[path = "integration/capacity.rs"]
mod capacity;
#[path = "integration/diagnostics.rs"]
mod diagnostics;
#[path = "integration/process.rs"]
mod process;
#[path = "integration/protocol.rs"]
mod protocol;
#[path = "integration/read_office.rs"]
mod read_office;
#[path = "integration/read_pdf.rs"]
mod read_pdf;
#[path = "integration/runtime_config.rs"]
mod runtime_config;
#[path = "integration/scope.rs"]
mod scope;
#[path = "integration/watchdog.rs"]
mod watchdog;

#[cfg(windows)]
#[path = "integration/windows.rs"]
mod windows;
