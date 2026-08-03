use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[test]
fn test_help_survives_reader_closing_pipe_early() {
    let mut producer = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .arg("--help")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn graphoxide --help");
    let stdout = producer.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .expect("read first help line");
    drop(reader);
    assert!(producer.wait().expect("wait for help producer").success());
}

#[test]
fn test_small_buffered_output_survives_reader_that_reads_nothing() {
    let mut producer = Command::new(env!("CARGO_BIN_EXE_graphoxide"))
        .arg("--version")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn graphoxide --version");
    drop(producer.stdout.take());
    assert!(producer
        .wait()
        .expect("wait for version producer")
        .success());
}
