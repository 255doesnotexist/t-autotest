use crate::needle::{Needle, NeedleManager};
use std::{
    env::current_dir,
    path::PathBuf,
    str::FromStr,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{self, Duration, Instant},
};
use t_binding::{MsgReq, MsgRes, MsgResError};
use t_config::{Config, ConsoleVNC};
use t_console::{key, ConsoleError, Serial, VNCEventReq, VNCEventRes, PNG, SSH, VNC};
use t_util::{get_time, AMOption};
use tracing::{debug, error, info, warn};

pub(crate) struct Server {
    pub(crate) msg_rx: Receiver<(MsgReq, Sender<MsgRes>)>,

    pub(crate) stop_rx: mpsc::Receiver<Sender<()>>,

    pub(crate) repo: Arc<Service>,
}

impl Server {
    pub fn start_non_blocking(self) {
        thread::spawn(move || {
            self.pool();
        });
    }

    fn try_stop(&self) -> bool {
        // stop on receive done signal
        if let Ok(tx) = self.stop_rx.try_recv() {
            info!(msg = "runner handler thread stopped");

            self.repo.ssh.map_ref(|c| c.stop());
            info!(msg = "ssh stopped");
            self.repo.serial.map_ref(|s| s.stop());
            info!(msg = "serial stopped");
            self.repo.vnc.map_ref(|s| s.stop());
            info!(msg = "vnc stopped");

            if let Err(e) = tx.send(()) {
                warn!(msg = "runner handler thread stopped", reason = ?e);
            }
            return true;
        }
        false
    }

    fn pool(&self) {
        // start script engine if in case mode
        info!(msg = "start msg handler thread");

        loop {
            let deadline = Instant::now() + Duration::from_millis(16);
            if self.try_stop() {
                break;
            }

            // handle msg
            match self.msg_rx.try_recv() {
                Ok((req, tx)) => {
                    let repo = self.repo.clone();
                    thread::spawn(move || {
                        let mut enable_log = true;
                        if matches!(req, MsgReq::VNC(t_binding::msg::VNC::TakeScreenShot)) {
                            enable_log = false;
                        }

                        if enable_log {
                            // info!(msg = "server recv req", req = ?req);
                        }
                        let res = repo.handle_req(req);

                        if enable_log {
                            // info!(msg = format!("sending res: {:?}", res));
                        }

                        if let Err(e) = tx.send(res) {
                            warn!(msg = "script engine receiver closed", reason = ?e);
                        }
                    });
                }
                Err(e) => match e {
                    mpsc::TryRecvError::Empty => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    mpsc::TryRecvError::Disconnected => {
                        warn!(msg = "request sender closed unexpected", reason = ?e);
                        break;
                    }
                },
            }
            thread::sleep(deadline - Instant::now());
        }
        info!(msg = "Runner loop stopped")
    }
}

pub(crate) struct Service {
    pub(crate) enable_screenshot: bool,

    pub(crate) config: AMOption<Config>,
    pub(crate) ssh: AMOption<SSH>,
    pub(crate) serial: AMOption<Serial>,
    pub(crate) vnc: AMOption<VNC>,
}

impl Service {
    fn save_screenshots(screenshot_rx: Receiver<(Arc<PNG>, String, Sender<()>)>, dir: PathBuf) {
        let path = dir;
        thread::spawn(move || {
            info!(msg = "vnc screenshot save thread started");
            // push ymdhms to path
            let path = path.join(get_time());

            let mut path = path;
            if let Err(e) = std::fs::create_dir_all(&path) {
                warn!(msg="create screenshot dir failed", reason=?e);
                return;
            }
            let mut i = 0;
            let mut last = None::<Arc<PNG>>;
            while let Ok((screen, name, done_tx)) = screenshot_rx.recv() {
                i += 1;
                if let Some(ref last) = last {
                    if last.cmp(screen.as_ref()) {
                        if let Err(e) = done_tx.send(()) {
                            warn!(msg="done send failed", reason=?e);
                        }
                        debug!(msg = "skip save screenshot, screen no change");
                        continue;
                    }
                }
                let p = screen.as_img();
                let image_name = format!("output-{i:05}-{}-{name}.png", get_time(),);
                path.push(&image_name);
                if let Err(e) = p.save(&path) {
                    warn!(msg="screenshot save failed", reason=?e);
                }
                path.pop();
                if let Err(e) = done_tx.send(()) {
                    warn!(msg="done send failed", reason=?e);
                }
                last = Some(screen);
            }
            let image_name = format!("output-{i:05}-{}-last.png", get_time(),);
            path.push(&image_name);
            if let Some(last) = last {
                if last.as_img().save(&path).is_err() {
                    warn!(msg="save last image failed", path=?path);
                }
            }
            info!(msg = "vnc screenshot save thread stopped");
        });
    }

    pub fn connect_with_config(&self, c: Config) -> Result<(), ConsoleError> {
        // init serial
        if let Some(c) = c.serial.clone() {
            self.serial.map_ref(|c| c.stop());
            match Serial::new(c) {
                Ok(s) => {
                    self.serial.set(Some(s));
                    info!(msg = "serial connect success");
                }
                Err(e) => {
                    error!(msg="serial connect failed", reason = ?e);
                    return Err(e);
                }
            }
        } else {
            self.serial.set(None);
        }

        // init ssh
        if let Some(c) = c.ssh.clone() {
            self.ssh.map_ref(|s| s.stop());
            match SSH::new(c) {
                Ok(s) => {
                    self.ssh.set(Some(s));
                    info!("ssh connect success");
                }
                Err(e) => {
                    error!(msg="ssh connect failed", reason = ?e);
                    return Err(e);
                }
            }
        } else {
            self.ssh.set(None);
        }

        // init vnc
        let build_vnc = move |vnc: ConsoleVNC| {
            let addr = format!("{}:{}", vnc.host, vnc.port)
                .parse()
                .map_err(|e| ConsoleError::NoConnection(format!("vnc addr is not valid, {}", e)))?;

            let tx = if let Some(screenshot_dir) = vnc.screenshot_dir.as_ref() {
                let (tx, rx) = mpsc::channel();
                Self::save_screenshots(rx, screenshot_dir.clone());
                Some(tx)
            } else {
                None
            };
            let vnc_client = VNC::connect(addr, vnc.password.clone(), tx)
                .map_err(|e| ConsoleError::NoConnection(e.to_string()))?;
            Ok::<VNC, ConsoleError>(vnc_client)
        };
        match c.vnc.clone().map(build_vnc) {
            Some(Ok(s)) => {
                self.vnc.set(Some(s));
                info!(msg = "vnc connect success");
            }
            Some(Err(e)) => {
                error!(msg = "vnc connect failed", reason = ?e);
                return Err(e);
            }
            None => {
                self.vnc.set(None);
            }
        }
        Ok(())
    }

    fn handle_req(&self, req: MsgReq) -> MsgRes {
        let res = match req {
            // common
            MsgReq::SetConfig { toml_str } => match Config::from_toml_str(&toml_str) {
                Ok(c) => match &mut self.connect_with_config(c.clone()) {
                    Ok(()) => {
                        self.config.set(Some(c));
                        MsgRes::Done
                    }
                    Err(e) => MsgRes::Error(MsgResError::String(format!(
                        "connect failed, reason = {}",
                        e
                    ))),
                },
                Err(e) => MsgRes::Error(MsgResError::String(format!(
                    "config invalid, reason = {}",
                    e
                ))),
            },
            MsgReq::GetConfig { key } => {
                let v = self.config.and_then_ref(|c| {
                    c.env
                        .as_ref()
                        .and_then(|e| e.get(&key).map(|v| v.to_string()))
                });
                MsgRes::ConfigValue(v)
            }
            // ssh
            MsgReq::SSHScriptRunSeperate { cmd, timeout: _ } => {
                let client = &self.ssh;
                let res = client
                    .map_mut(|c| c.exec_seperate(&cmd))
                    .unwrap_or(Ok((-1, "no ssh".to_string())))
                    .map_err(|_| MsgResError::Timeout);
                match res {
                    Ok((code, value)) => MsgRes::ScriptRun { code, value },
                    Err(e) => MsgRes::Error(e),
                }
            }
            MsgReq::ScriptRun {
                cmd,
                console,
                timeout,
            } => {
                let res = match (console, self.ssh.is_some(), self.serial.is_some()) {
                    (None | Some(t_binding::TextConsole::Serial), _, true) => self
                        .serial
                        .map_mut(|c| c.exec(timeout, &cmd))
                        .unwrap_or(Ok((1, "no serial".to_string())))
                        .map_err(|_| MsgResError::Timeout),
                    (None | Some(t_binding::TextConsole::SSH), true, _) => self
                        .ssh
                        .map_mut(|c| c.exec(timeout, &cmd))
                        .unwrap_or(Ok((-1, "no ssh".to_string())))
                        .map_err(|_| MsgResError::Timeout),
                    _ => Err(MsgResError::String("no console supported".to_string())),
                };
                match res {
                    Ok((code, value)) => MsgRes::ScriptRun { code, value },
                    Err(e) => MsgRes::Error(e),
                }
            }
            MsgReq::WriteString {
                console,
                s,
                timeout,
            } => {
                if let Err(e) = match (console, self.ssh.is_some(), self.serial.is_some()) {
                    (None | Some(t_binding::TextConsole::Serial), _, true) => self
                        .serial
                        .map_mut(|c| c.write_string(&s, timeout))
                        .expect("no serial")
                        .map_err(|_| MsgResError::Timeout),
                    (None | Some(t_binding::TextConsole::SSH), true, _) => self
                        .ssh
                        .map_mut(|c| c.write_string(&s, timeout))
                        .expect("no ssh")
                        .map_err(|_| MsgResError::Timeout),
                    _ => Err(MsgResError::String("no console supported".to_string())),
                } {
                    MsgRes::Error(e)
                } else {
                    MsgRes::Done
                }
            }
            MsgReq::WaitString {
                console,
                s,
                timeout,
            } => {
                if let Err(e) = match (console, self.ssh.is_some(), self.serial.is_some()) {
                    (None | Some(t_binding::TextConsole::Serial), _, true) => self
                        .serial
                        .map_mut(|c| c.wait_string(timeout, &s))
                        .expect("no serial")
                        .map_err(|_| MsgResError::Timeout),
                    (None | Some(t_binding::TextConsole::SSH), true, _) => self
                        .ssh
                        .map_mut(|c| c.wait_string(timeout, &s))
                        .expect("no ssh")
                        .map_err(|_| MsgResError::Timeout),
                    _ => Err(MsgResError::String("no console supported".to_string())),
                } {
                    MsgRes::Error(e)
                } else {
                    MsgRes::Done
                }
            }
            MsgReq::VNC(e) => self.handle_vnc_req(e),
        };
        res
    }

    pub fn handle_vnc_req(&self, req: t_binding::msg::VNC) -> MsgRes {
        let nmg = NeedleManager::new(
            self.config
                .and_then_ref(|c| {
                    c.vnc.as_ref().and_then(|vnc| {
                        vnc.needle_dir
                            .as_ref()
                            .and_then(|d| PathBuf::from_str(d).ok())
                    })
                })
                .unwrap_or(current_dir().unwrap()),
        );
        let mut take_screenshot = false;
        if let Some(res) = self.vnc.map_ref(|c| {
            let screenshotname;
            let res = match req {
                t_binding::msg::VNC::TakeScreenShot => {
                    take_screenshot = false;
                    screenshotname = "user";
                    match c.send(VNCEventReq::TakeScreenShot(
                        screenshotname.to_string(),
                    )) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::GetScreenShot => {
                    screenshotname = "user";
                    match c.send(VNCEventReq::GetScreenShot) {
                        Ok(VNCEventRes::Screen(res)) => MsgRes::Screenshot(res),
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::Refresh => {
                    screenshotname = "refresh";
                    match c.send(VNCEventReq::Refresh) {
                        Ok(VNCEventRes::Screen(res)) => MsgRes::Screenshot(res),
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::CheckScreen {
                    tag,
                    threshold,
                    timeout,
                    click,
                    r#move,
                    delay,
                } => {
                    take_screenshot = false;
                    screenshotname = "checkscreen";
                    let deadline = time::Instant::now() + timeout;
                    let mut similarity: f32 = 0.;
                    let mut i = 0;
                    'res: loop {
                        i += 1;
                        if Instant::now() > deadline {
                            let msg = "match timeout";
                            info!(msg = msg, tag = tag, similarity = similarity);
                            break 'res MsgRes::Error(MsgResError::String(
                                msg.to_string()
                            ));
                        }
                        match c.send(VNCEventReq::GetScreenShot) {
                            Ok(VNCEventRes::Screen(s)) => {
                                let Some(needle) = nmg.load(&tag) else {
                                    let msg = "assert screen failed, needle file not found";
                                    error!(msg = msg, tag = tag);
                                    if self.enable_screenshot && c.send(VNCEventReq::TakeScreenShot(format!(
                                        "{screenshotname}-{i}-failed-noneedle"
                                    )))
                                    .is_err()
                                    {
                                        warn!("take screenshot failed, vnc server may stopped unexpectedly")
                                    }
                                    if Instant::now() > deadline {
                                        break 'res MsgRes::Error(MsgResError::String(
                                            msg.to_string()
                                        ));
                                    }
                                    thread::sleep(Duration::from_millis(1000));
                                    continue;
                                };

                                let (res_similarity, needle_match) = Needle::cmp(
                                    &s,
                                    &needle,
                                    Some(threshold),
                                ) ;

                                similarity = res_similarity;

                                if needle_match {
                                    info!(
                                        msg = "match success",
                                        tag = tag,
                                        similarity = similarity
                                    );
                                    if let Some(delay) = delay {
                                        thread::sleep(delay);
                                    }
                                    if click || r#move {
                                        for area in needle.config.areas {
                                            if let Some(point) = area.click {
                                                let x = point.left + area.left;
                                                let y = point.top + area.top;
                                                    if r#move && !matches!(c.send(VNCEventReq::MouseMove(x, y)), Ok(VNCEventRes::Done)) {
                                                        let msg ="check screen success, but mouse move failed";
                                                        warn!(msg = msg);
                                                        break 'res MsgRes::Error(MsgResError::String(msg.to_string()));
                                                }
                                                if click {
                                                    thread::sleep(Duration::from_millis(1000));
                                                    if !matches!(c.send(VNCEventReq::MouseMove(x, y)), Ok(VNCEventRes::Done)) {
                                                        let msg ="check screen success, but mouse move failed";
                                                        warn!(msg = msg);
                                                        break 'res MsgRes::Error(MsgResError::String(msg.to_string()));
                                                    }
                                                    thread::sleep(Duration::from_millis(1000));
                                                    if !matches!(c.send(VNCEventReq::MouseClick(1)), Ok(VNCEventRes::Done)) {
                                                        let msg ="check screen and mouse move success, but mouse click failed";
                                                        warn!(msg = msg);
                                                        break 'res MsgRes::Error(MsgResError::String(msg.to_string()));
                                                    }
                                                    thread::sleep(Duration::from_millis(1000));
                                                }
                                                break;
                                            }
                                        }
                                            if !r#move && !matches!(c.send(VNCEventReq::MouseHide), Ok(VNCEventRes::Done)) {
                                                let msg ="check screen success, but mouse hide after click failed";
                                                warn!(msg = msg);
                                                break 'res MsgRes::Error(MsgResError::String(msg.to_string()));
                                            }
                                    }
                                    break 'res MsgRes::Done;
                                } else {
                                    if  self.enable_screenshot && c.send(VNCEventReq::TakeScreenShot(
                                        format!(
                                            "{screenshotname}-{i}-success"
                                        ),
                                    )).is_err() {
                                        warn!("take screenshot failed, vnc server may stopped unexpectedly")
                                    }
                                    warn!(msg = "match failed", tag = tag, similarity = similarity);
                                }
                            }
                            Ok(_) => {
                                warn!(msg = "invalid msg type");
                            }
                            Err(_e) => break MsgRes::Error(MsgResError::Timeout),
                        }
                        thread::sleep(Duration::from_millis(200));
                    }
                }
                t_binding::msg::VNC::MouseMove { x, y } => {
                    screenshotname = "mousemove";
                    match c.send(VNCEventReq::MouseMove(x, y)) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::MouseDrag { x, y } => {
                    screenshotname = "mousedrag";
                    match c.send(VNCEventReq::MouseDrag(x, y)) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::MouseHide => {
                    screenshotname = "mousehide";
                    match c.send(VNCEventReq::MouseHide) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::MouseClick
                | t_binding::msg::VNC::MouseRClick => {
                    screenshotname = "mouseclick";
                    let button = match req {
                        t_binding::msg::VNC::MouseClick => 1,
                        t_binding::msg::VNC::MouseRClick => 1 << 2,
                        _ => unreachable!(),
                    };
                    match c.send(VNCEventReq::MouseClick(button)) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::MouseKeyDown(down) => {
                    screenshotname =
                        if down { "mousekeydown" } else { "mousekeyup" };
                    match c.send(if down {
                        VNCEventReq::MoveDown(1)
                    } else {
                        VNCEventReq::MoveUp(1)
                    }) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::SendKey(s) => {
                    screenshotname = "sendkey";
                    let mut keys = Vec::new();
                    if s == "-" { keys.push(b'-' as u32)} else {
                        let parts = s.split('-');
                        for part in parts {
                            if let Some(key) = key::from_str(part) {
                                keys.push(key);
                            }
                        }
                    }
                    match c.send(VNCEventReq::SendKey { keys }) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
                t_binding::msg::VNC::TypeString(s) => {
                    screenshotname = "typestring";
                    match c.send(VNCEventReq::TypeString(s)) {
                        Ok(VNCEventRes::Done) => MsgRes::Done,
                        _ => MsgRes::Error(MsgResError::Timeout),
                    }
                }
            };
            // take a screenshot after the action
            if self.enable_screenshot && c.send(VNCEventReq::TakeScreenShot(screenshotname.to_string())).is_err() {
                warn!(msg="take screenshot failed");
            }
            res
        }) {
            res
        } else {
            MsgRes::Error(MsgResError::String("no vnc".to_string()))
        }
    }
}

#[cfg(test)]
mod test {

    #[test]
    fn test_runner() {}
}
