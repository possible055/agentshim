use std::{
    collections::{BTreeMap, VecDeque},
    env,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};

const DEFAULT_ITERATIONS: usize = 100;
const EXTENDED_ITERATIONS: usize = 1_000;
const BURST_QUIET_MS: u64 = 2_100;

#[path = "common/mod.rs"]
pub mod common;

#[path = "resource_soak/aggregate.rs"]
mod aggregate;
#[path = "resource_soak/pending.rs"]
mod pending;
#[path = "resource_soak/single_instance.rs"]
mod single_instance;
#[path = "resource_soak/support.rs"]
mod support;
