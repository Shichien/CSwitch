use std::error::Error;
use std::time::Duration;

// reqwest's system-proxy support covers Windows/macOS; native roots include enterprise CAs.
// Loopback control traffic is handled separately and always disables proxies.
pub(crate) fn blocking_builder(
    timeout: Duration,
) -> Result<reqwest::blocking::ClientBuilder, Box<dyn Error>> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = desktop_proxy()? {
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

pub(crate) fn async_builder() -> Result<reqwest::ClientBuilder, Box<dyn Error>> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = desktop_proxy()? {
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

fn desktop_proxy() -> Result<Option<reqwest::Proxy>, Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        if [
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|value| !value.trim().is_empty()))
        {
            return Ok(None);
        }
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            return Ok(None);
        }
        let mode = match std::process::Command::new("gsettings")
            .args(["get", "org.gnome.system.proxy", "mode"])
            .output()
        {
            Ok(output) if output.status.success() => String::from_utf8(output.stdout)?,
            Ok(_) => return Ok(None), // not a GNOME settings installation
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("读取桌面代理失败：{error}").into()),
        };
        if mode.trim() == "'auto'" {
            return Err(
                "当前桌面使用 PAC 自动代理，请通过 HTTPS_PROXY 指定用于 CSwitch 的固定代理地址"
                    .into(),
            );
        }
        if mode.trim() == "'manual'" {
            let read = |key| -> Result<String, Box<dyn Error>> {
                let output = std::process::Command::new("gsettings")
                    .args(["get", "org.gnome.system.proxy.https", key])
                    .output()?;
                if !output.status.success() {
                    return Err("读取桌面 HTTPS 代理失败".into());
                }
                Ok(String::from_utf8(output.stdout)?
                    .trim()
                    .trim_matches('\'')
                    .to_string())
            };
            let host = read("host")?;
            if !host.is_empty() {
                let port: u16 = read("port")?.parse()?;
                let proxy = reqwest::Proxy::https(format!("http://{host}:{port}"))?;
                return Ok(Some(proxy));
            }
        }
    }
    Ok(None)
}
