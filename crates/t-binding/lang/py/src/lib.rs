#![allow(unused)]
use pyo3::{
    exceptions::{self, PyException, PyTypeError},
    prelude::*,
};
use std::{env, time::Duration};
use t_binding::{api, ApiError};
use t_config::{Config, ConsoleSSH};
use t_console::SSH;
use t_runner::Driver as InnerDriver;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

pyo3::create_exception!(defaultmodule, DriverException, PyException);
pyo3::create_exception!(defaultmodule, AssertException, PyException);
pyo3::create_exception!(defaultmodule, TimeoutException, PyException);

fn into_pyerr(e: ApiError) -> PyErr {
    match e {
        ApiError::ServerStopped => {
            DriverException::new_err("server stopped, maybe needle not found")
        }
        ApiError::ServerInvalidResponse => {
            DriverException::new_err("server return invalid response, please open an issue")
        }
        ApiError::Timeout => TimeoutException::new_err("timeout"),
        ApiError::AssertFailed => AssertException::new_err("assert failed"),
    }
}

/// Entrypoint, A Python module implemented in Rust.
#[pymodule]
fn pyautotest(py: Python, m: &PyModule) -> PyResult<()> {
    ctrlc::set_handler(|| std::process::exit(2)).unwrap();
    init_logger();

    tracing::info!("pyautotest module initialized");
    m.add_class::<Driver>()?;
    Ok(())
}

fn init_logger() {
    let log_level = match env::var("RUST_LOG") {
        Ok(l) => match l.as_str() {
            "trace" => Level::TRACE,
            "debug" => Level::DEBUG,
            "warn" => Level::WARN,
            "error" => Level::ERROR,
            "info" => Level::INFO,
            _ => return,
        },
        _ => return,
    };

    let format = tracing_subscriber::fmt::format()
        .without_time()
        .with_target(false)
        .with_level(true)
        .with_source_location(true)
        .compact();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .event_format(format)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

#[pyclass]
struct Driver {
    inner: InnerDriver,
}

#[pymethods]
impl Driver {
    #[new]
    fn __init__(config: String) -> PyResult<Self> {
        let config =
            Config::from_toml_str(&config).map_err(|e| DriverException::new_err(e.to_string()))?;
        let mut runner = InnerDriver::new(config.clone()).map_err(|e| {
            DriverException::new_err(format!("driver init failed, reason: [{}]", e))
        })?;
        runner.start();
        Ok(Self { inner: runner })
    }

    // ssh
    fn new_ssh(&self) -> PyResult<DriverSSH> {
        DriverSSH::new(self.inner.config.console.ssh.clone())
    }

    fn stop(&mut self) {
        self.inner.stop();
    }

    fn sleep(&self, miles: i32) {
        api::sleep(miles as u64);
    }

    fn get_env(&self, key: String) -> PyResult<Option<String>> {
        api::get_env(key).map_err(into_pyerr)
    }

    fn assert_script_run_global(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::assert_script_run_global(cmd, timeout).map_err(into_pyerr)
    }

    fn script_run_global(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::script_run_global(cmd, timeout)
            .map(|v| v.1)
            .map_err(into_pyerr)
    }

    fn write_string(&self, s: String) -> PyResult<()> {
        api::write_string(s).map_err(into_pyerr)
    }

    fn wait_string_ntimes(&self, s: String, n: i32, timeout: i32) -> PyResult<()> {
        api::wait_string_ntimes(s, n, timeout).map_err(into_pyerr)
    }

    // ssh
    fn ssh_assert_script_run_global(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::ssh_assert_script_run_global(cmd, timeout).map_err(into_pyerr)
    }

    fn ssh_script_run_global(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::ssh_script_run_global(cmd, timeout)
            .map(|v| v.1)
            .map_err(into_pyerr)
    }

    fn ssh_write_string(&self, s: String) {
        api::ssh_write_string(s);
    }

    fn ssh_assert_script_run_seperate(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::ssh_assert_script_run_seperate(cmd, timeout).map_err(into_pyerr)
    }

    // serial
    fn serial_assert_script_run_global(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::serial_assert_script_run_global(cmd, timeout).map_err(into_pyerr)
    }

    fn serial_script_run_global(&self, cmd: String, timeout: i32) -> PyResult<String> {
        api::serial_script_run_global(cmd, timeout)
            .map(|v| v.1)
            .map_err(into_pyerr)
    }

    fn serial_write_string(&self, s: String) {
        api::serial_write_string(s);
    }

    // vnc
    fn assert_screen(&self, tag: String, timeout: i32) -> PyResult<()> {
        api::vnc_assert_screen(tag, timeout).map_err(into_pyerr)
    }

    fn check_screen(&self, tag: String, timeout: i32) -> PyResult<bool> {
        api::vnc_check_screen(tag, timeout).map_err(into_pyerr)
    }

    fn mouse_click(&self) -> PyResult<()> {
        api::vnc_mouse_click().map_err(into_pyerr)
    }

    fn mouse_move(&self, x: i32, y: i32) -> PyResult<()> {
        api::vnc_mouse_move(x as u16, y as u16).map_err(into_pyerr)
    }

    fn mouse_hide(&self) -> PyResult<()> {
        api::vnc_mouse_hide().map_err(into_pyerr)
    }
}

#[pyclass]
struct DriverSSH {
    inner: SSH,
}

impl DriverSSH {
    pub fn new(c: ConsoleSSH) -> PyResult<Self> {
        Ok(Self {
            inner: SSH::new(c).map_err(|e| DriverException::new_err(e.to_string()))?,
        })
    }
}

#[pymethods]
impl DriverSSH {
    fn get_tty(&self) -> String {
        self.inner.tty()
    }

    fn assert_script_run(&mut self, cmd: String, timeout: u64) -> PyResult<String> {
        let Ok(v) = self.inner.exec_global(Duration::from_millis(timeout), &cmd) else {
            return Err(TimeoutException::new_err("assert script run timeout"));
        };
        if v.0 != 0 {
            return Err(AssertException::new_err(format!(
                "return code should be 0, but got {}",
                v.0
            )));
        }
        Ok(v.1)
    }
}

#[cfg(test)]
mod test {
    use pyo3::types::PyModule;

    #[test]
    fn test_pyo3() {
        #[pyo3::pyfunction]
        fn add(a: i64, b: i64) -> i64 {
            // hello();
            a + b
        }

        pyo3::Python::with_gil(|py| -> pyo3::PyResult<()> {
            let module_testapi_name = "testapi".to_string();
            let module_testapi = PyModule::new(py, &module_testapi_name)?;
            module_testapi.add_function(pyo3::wrap_pyfunction!(add, module_testapi)?)?;

            // Import and get sys.modules
            let sys = PyModule::import(py, "sys")?;
            let py_modules: &pyo3::types::PyDict = sys.getattr("modules")?.downcast()?;

            // Insert foo into sys.modules
            py_modules.set_item(&module_testapi_name, module_testapi)?;

            // Now we can import + run our python code
            pyo3::Python::run(py, "import testapi; testapi.add(1, 2)", None, None).unwrap();

            // let res = py.eval("import testapi; testapi.add(1, 2)", None, None)?;
            // assert!(res.extract::<i64>()? == 4);
            Ok(())
        })
        .unwrap()
    }
}