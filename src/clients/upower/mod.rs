mod dbus;

use crate::clients::ClientResult;
use crate::register_fallible_client;
use dbus::UPowerProxy;
use std::sync::Arc;
use zbus::fdo::PropertiesProxy;
use zbus::proxy::CacheProperties;

pub use dbus::BatteryState;

pub async fn create_proxies() -> ClientResult<Vec<Arc<PropertiesProxy<'static>>>> {
    let dbus = Box::pin(zbus::Connection::system()).await?;

    let device_proxy = UPowerProxy::new(&dbus).await?;
    let device_paths = device_proxy.enumerate_devices().await?;
    let mut proxies = Vec::new();

    for path in device_paths {
        let proxy = PropertiesProxy::builder(&dbus)
            .destination("org.freedesktop.UPower")
            .expect("failed to set proxy destination address")
            .path(path)
            .expect("failed to set proxy path")
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        proxies.push(Arc::new(proxy));
    }

    Ok(Arc::new(proxies))
}

register_fallible_client!(Vec<Arc<PropertiesProxy<'static>>>, upower);
