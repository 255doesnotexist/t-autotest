use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::mpsc::{self, channel, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use image::EncodableLayout;
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub enum Req {
    Write(Vec<u8>),
    Read,
    Dump,
    Stop,
}

#[derive(Debug)]
pub enum Res {
    Done,
    Value(Vec<u8>),
}

pub struct EvLoopCtl {
    req_tx: Sender<(Req, Sender<Res>)>,
}

impl EvLoopCtl {
    pub fn new<T: Read + Write + Send + 'static>(conn: T, log_file: Option<PathBuf>) -> Self {
        let req_tx = EventLoop::spawn(conn, log_file);
        Self { req_tx }
    }

    pub fn send(&self, req: Req) -> Result<Res, mpsc::RecvError> {
        let (tx, rx) = channel();
        if let Err(e) = self.req_tx.send((req, tx)) {
            error!("evloop receiver closed, connection may be lost: {}", e);
            return Err(mpsc::RecvError {});
        }
        rx.recv()
    }
}

struct EventLoop<T> {
    conn: T,
    req_rx: Receiver<(Req, Sender<Res>)>,
    history: Vec<u8>,
    log_file: Option<File>,
    last_read_index: usize,
    buffer: Vec<u8>,
}

impl<T> EventLoop<T>
where
    T: Read + Write + Send + 'static,
{
    pub fn spawn(conn: T, log_file: Option<PathBuf>) -> Sender<(Req, Sender<Res>)> {
        let log_file = if let Some(ref log_file) = log_file {
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(log_file)
                .expect("Failed to open file");
            Some(file)
        } else {
            None
        };

        let (req_tx, req_rx) = mpsc::channel();

        thread::spawn(move || {
            Self {
                conn,
                req_rx,
                log_file,
                history: Vec::new(),
                last_read_index: 0,
                buffer: vec![0u8; 4096],
            }
            .pool();
        });
        req_tx
    }

    fn pool(&mut self) {
        let min_interval = Duration::from_millis(1000);
        let mut next_round = Instant::now() + min_interval;
        'out: loop {
            // handle tty output
            if let Err(e) = self.try_read_buffer() {
                error!(msg="evloop can't continue", reason = ?e);
                break 'out;
            }

            // don't return too fast
            if Instant::now() < next_round {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            next_round = Instant::now() + min_interval;

            // handle user read, write request
            match self.req_rx.try_recv() {
                Ok((req, tx)) => {
                    // handle stop
                    if matches!(req, Req::Stop) {
                        let _ = tx.send(Res::Done);
                        break 'out;
                    }
                    let Ok(res) = self.handle_req(req) else {
                        info!("stopped while blocking");
                        break 'out;
                    };
                    if let Err(e) = tx.send(res) {
                        warn!("req sender side closed before recv response: {}", e);
                        continue;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // ignore empty
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // sender closed, evloop should stop here
                    break;
                }
            }
        }
    }

    // block until receive new buffer, try receive only once
    fn handle_req(&mut self, req: Req) -> Result<Res, ()> {
        match req {
            Req::Stop => {
                // should be handled before
                Ok(Res::Done)
            }
            Req::Write(msg) => {
                if let Err(e) = self.conn.write_all(msg.as_bytes()) {
                    error!(msg = "write failed, connection may be broken", reason = ?e);
                    return Err(());
                }
                if let Err(e) = self.conn.flush() {
                    error!(msg = "flush failed, connection may be broken", reason = ?e);
                }
                debug!(msg = "write done");
                Ok(Res::Done)
            }
            Req::Read => Ok(Res::Value(self.consume_buffer())),
            Req::Dump => Ok(Res::Value(self.history.clone())),
        }
    }

    fn try_read_buffer(&mut self) -> Result<Vec<u8>, io::Error> {
        match self.conn.read(&mut self.buffer) {
            Ok(n) => {
                if n != 0 {
                    let received = &self.buffer[0..n];
                    self.history.extend(received);

                    if let Some(ref mut log_file) = self.log_file {
                        log_file
                            .write_all(received)
                            .expect("unable to store console output");
                    }
                    return Ok(received.to_vec());
                }
                Ok(Vec::new())
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                // ignore timeout
                Ok(Vec::new())
            }
            Err(e) => {
                error!(msg = "connection may be broken", reason = ?e);
                Err(e)
            }
        }
    }

    fn consume_buffer(&mut self) -> Vec<u8> {
        if self.last_read_index == self.history.len() {
            return Vec::new();
        }
        let res = &self.history[self.last_read_index..];
        self.last_read_index = self.history.len();
        res.to_vec()
    }
}
