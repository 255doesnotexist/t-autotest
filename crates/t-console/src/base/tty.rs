use super::evloop::{EvLoopCtl, Req, Res};
use crate::{term::Term, ConsoleError};
use parking_lot::Mutex;
use std::{
    marker::PhantomData,
    sync::mpsc::Receiver,
    thread,
    time::{Duration, Instant},
};
use tracing::{debug, error, info};

type Result<T> = std::result::Result<T, ConsoleError>;

struct State {
    // store all tty output bytes
    history: Vec<u8>,
    // used by regex search history start
    last_buffer_start: usize,
}

pub struct TtySetting {
    pub disable_echo: bool,
    pub linebreak: String,
}

pub struct Tty<T: Term> {
    // interface for communicate with tty file
    ctl: EvLoopCtl,
    stop_rx: Mutex<Receiver<()>>,
    state: Mutex<State>,
    setting: TtySetting,
    // Term decide how to decode output bytes
    phantom: PhantomData<T>,
}

enum ConsumeAction<T> {
    BreakValue(T),
    Continue,
    #[allow(unused)]
    Cancel,
}

impl<Tm> Tty<Tm>
where
    Tm: Term,
{
    pub fn new(ctl: EvLoopCtl, stop_rx: Receiver<()>, setting: TtySetting) -> Self {
        Self {
            ctl,
            stop_rx: Mutex::new(stop_rx),
            state: Mutex::new(State {
                history: Vec::new(),
                last_buffer_start: 0,
            }),
            setting,
            phantom: PhantomData {},
        }
    }

    pub fn stop_evloop(&self) {
        self.ctl.stop();
    }

    fn try_handle_stop_signal(&self) -> bool {
        // stop on receive done signal
        self.stop_rx.lock().try_recv().is_ok()
    }

    pub fn write(&self, s: &[u8], timeout: Duration) -> Result<()> {
        self.ctl
            .send_timeout(Req::Write(s.to_vec()), timeout)
            .map_err(|_| ConsoleError::Timeout)?;
        Ok(())
    }

    pub fn write_string(&self, s: &str, timeout: Duration) -> Result<()> {
        info!(msg = "write_string", s = s);
        self.write(s.as_bytes(), timeout)?;
        Ok(())
    }

    pub fn wait_string(&mut self, timeout: Duration, pattern: &str) -> Result<String> {
        info!(msg = "wait_string", pattern = pattern);
        self.comsume_buffer_and_map(timeout, |buffer, new| {
            {
                let buffer_str = Tm::parse_and_strip(buffer);
                let new_str = Tm::parse_and_strip(new);
                let res = count_substring(&buffer_str, pattern, 1);
                info!(
                    msg = "wait_string",
                    pattern = pattern,
                    res = res,
                    new_buffer = new_str,
                );
                res.then_some(buffer_str)
            }
            .map_or(ConsumeAction::Continue, ConsumeAction::BreakValue)
        })
    }

    pub fn exec(&mut self, timeout: Duration, cmd: &str) -> Result<(i32, String)> {
        info!(msg = "exec", cmd = cmd);
        let enter_input: &'static str = "\r";

        // wait for prompt show, cmd may write too fast before prompt show, which will broken regex
        std::thread::sleep(Duration::from_millis(70));

        // prepare
        let nanoid = nanoid::nanoid!(6);

        let res_flag_sep = "-";

        let (cmd, match_left) = if self.setting.disable_echo {
            // echo -$?$nanoid; cmd; echo $?$nanoid\r
            let cmd = format!("echo {nanoid}; {cmd}; echo -$?{nanoid}{}", enter_input);
            // $nanoid\nresult-0$nanoid\n
            let match_left = format!("{nanoid}{}", &self.setting.linebreak);
            (cmd, match_left)
        } else {
            // cmd; echo -$?$nanoid\r
            let cmd = format!("{cmd}; echo {}$?{nanoid}{}", res_flag_sep, enter_input);
            // cmd; echo -$?$nanoid\rresult-0$nanoid\n
            let match_left = format!("{nanoid}{}{}", &self.setting.linebreak, enter_input);
            (cmd, match_left)
        };

        // result-0$nanoid\n
        let match_right = &format!("{nanoid}{}", &self.setting.linebreak);

        // run command
        self.write_string(&cmd, timeout)?;

        // wait output
        let deadline = Instant::now() + timeout;
        self.comsume_buffer_and_map(deadline - Instant::now(), |buffer, new| {
            // find target pattern from buffer
            let buffer_str = Tm::parse_and_strip(buffer);
            let new_str = Tm::parse_and_strip(new);
            info!(
                msg = "recv string",
                nanoid = nanoid,
                buffer_len = buffer.len(),
                new_buffer = new_str,
            );

            let Ok(catched_output) =
                t_util::assert_capture_between(&buffer_str, &match_left, match_right)
            else {
                return ConsumeAction::BreakValue((1, "invalid consume regex".to_string()));
            };
            match catched_output {
                Some((_pos, v)) => {
                    info!(msg = "catched_output", nanoid = nanoid, catched_output = v,);
                    if let Some((res, flag)) = v.rsplit_once(res_flag_sep) {
                        info!(
                            msg = "catched_output_splited",
                            nanoid = nanoid,
                            flag = flag,
                            res = res
                        );
                        if let Ok(flag) = flag.parse::<i32>() {
                            return ConsumeAction::BreakValue((flag, res.to_string()));
                        }
                    } else {
                        // some command doesn't print, like 'sleep'
                        if let Ok(flag) = v.parse::<i32>() {
                            return ConsumeAction::BreakValue((flag, "".to_string()));
                        }
                    }
                    ConsumeAction::BreakValue((1, v))
                }
                None => {
                    debug!(msg = "consume buffer continue");
                    ConsumeAction::Continue
                }
            }
        })
    }

    fn comsume_buffer_and_map<T>(
        &self,
        timeout: Duration,
        f: impl Fn(&[u8], &[u8]) -> ConsumeAction<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;

        let mut buffer_len = 0;
        loop {
            if self.try_handle_stop_signal() {
                return Err(ConsoleError::Cancel);
            }

            tracing::info!(msg = "deadline", deadline = ?(deadline - Instant::now()));
            // handle timeout
            if Instant::now() > deadline {
                break;
            }

            thread::sleep(Duration::from_millis(1000));

            // read buffer
            let res = self
                .ctl
                .send_timeout(Req::Read, Duration::from_millis(1000));
            match res {
                Ok(Res::Value(ref recv)) => {
                    if recv.is_empty() {
                        continue;
                    }

                    let mut state = self.state.lock();
                    // save to history
                    state.history.extend(recv);
                    buffer_len += recv.len();

                    debug!(
                        msg = "event loop recv",
                        sum_buffer_len = state.history.len() - state.last_buffer_start,
                        last_buffer_start = state.last_buffer_start,
                        old_buffer_len = state.history.len() - buffer_len,
                        new_buffer_len = buffer_len,
                        new_buffer_acc = recv.len(),
                    );

                    // find target pattern
                    let res = f(&state.history[state.last_buffer_start..], recv);

                    match res {
                        ConsumeAction::BreakValue(v) => {
                            // cut from last find
                            debug!(msg = "buffer cut");
                            state.last_buffer_start = state.history.len() - buffer_len;
                            return Ok(v);
                        }
                        ConsumeAction::Continue => {
                            continue;
                        }
                        ConsumeAction::Cancel => {
                            return Err(ConsoleError::Cancel);
                        }
                    }
                }
                Ok(res) => {
                    error!(msg = "invalid msg varient", res = ?res);
                    break;
                }
                Err(e) => match e {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {}
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        error!(msg = "recv failed");
                        break;
                    }
                },
            }
        }
        Err(ConsoleError::Timeout)
    }
}

fn count_substring(s: &str, substring: &str, n: usize) -> bool {
    let mut count = 0;
    let mut start = 0;

    while let Some(pos) = s[start..].find(substring) {
        count += 1;
        if count == n {
            return true;
        }
        start += pos + substring.len();
    }

    false
}
