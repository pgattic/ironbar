use color_eyre::Result;
use futures_lite::stream::StreamExt;
use gtk::{Button, prelude::*};
use gtk::{Label, Orientation};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write;
use tokio::sync::mpsc;
use zbus;
use zbus::fdo::PropertiesProxy;

use crate::channels::{AsyncSenderExt, BroadcastReceiverExt};
use crate::clients::upower::BatteryState;
use crate::config::{CommonConfig, LayoutConfig};
use crate::gtk_helpers::{IronbarGtkExt, IronbarLabelExt};
use crate::modules::PopupButton;
use crate::modules::{
    Module, ModuleInfo, ModuleParts, ModulePopup, ModuleUpdateEvent, WidgetContext,
};
use crate::{module_impl, spawn};

const DAY: i64 = 24 * 60 * 60;
const HOUR: i64 = 60 * 60;
const MINUTE: i64 = 60;

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpowerModule {
    /// The format string to use for the widget button label.
    /// For available tokens, see [below](#formatting-tokens).
    ///
    /// **Default**: `{percentage}%`
    #[serde(default = "default_format")]
    format: String,

    /// The size to render the icon at, in pixels.
    ///
    /// **Default**: `24`
    #[serde(default = "default_icon_size")]
    icon_size: i32,

    // -- Common --
    /// See [layout options](module-level-options#layout)
    #[serde(default, flatten)]
    layout: LayoutConfig,

    /// See [common options](module-level-options#common-options).
    #[serde(flatten)]
    pub common: Option<CommonConfig>,
}

fn default_format() -> String {
    String::from("{percentage}%")
}

const fn default_icon_size() -> i32 {
    24
}

#[derive(Clone, Debug)]
pub struct UpowerProperties {
    percentage: f64,
    icon_name: String,
    state: BatteryState,
    time_to_full: i64,
    time_to_empty: i64,
}

use std::sync::Arc;
use tokio::sync::Mutex;

impl Module<Button> for UpowerModule {
    type SendMessage = HashMap<String, UpowerProperties>;
    type ReceiveMessage = ();

    module_impl!("upower");

    fn spawn_controller(
        &self,
        _info: &ModuleInfo,
        context: &WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        _rx: mpsc::Receiver<Self::ReceiveMessage>,
    ) -> Result<()> {

        let display_proxies: Arc<Vec<Arc<PropertiesProxy<'_>>>> = context.try_client::<Vec<Arc<PropertiesProxy>>>()?;
        let tx = context.tx.clone();
        let device_interface_name = zbus::names::InterfaceName::from_static_str("org.freedesktop.UPower.Device")
            .expect("failed to create zbus InterfaceName");

        let properties_map = Arc::new(Mutex::new(HashMap::new()));

        let init_props = {
            let properties_map = properties_map.clone();
            let tx = tx.clone();
            let disp_proxies = display_proxies.clone();
            let device_int_name = device_interface_name.clone();

            async move {

                for display_proxy in disp_proxies.iter() {
                    let proxy = display_proxy.clone();
                    let raw_props = proxy.get_all(device_int_name.clone()).await?;
                    // println!("{:?}", raw_props);

                    let bat_type = raw_props["Type"]
                        .downcast_ref::<u32>()
                        .expect("expected Type: u32 in HashMap of all properties");

                    // We only want to track signals from batteries (as opposed to Power Supplies)
                    // Remove or modify this conditional if we ever want to monitor more
                    if bat_type != 2 {
                        continue;
                    }

                    let native_path = raw_props["NativePath"]
                        .downcast_ref::<&str>()
                        .expect("expected NativePath: str in HashMap of all properties")
                        .to_string();

                    let percentage = raw_props["Percentage"]
                        .downcast_ref::<f64>()
                        .expect("expected percentage: f64 in HashMap of all properties");
                    let icon_name = raw_props["IconName"]
                        .downcast_ref::<&str>()
                        .expect("expected IconName: str in HashMap of all properties")
                        .to_string();
                    let state = u32_to_battery_state(
                        raw_props["State"]
                            .downcast_ref::<u32>()
                            .expect("expected State: u32 in HashMap of all properties"),
                    )
                        .unwrap_or(BatteryState::Unknown);
                    let time_to_full = raw_props["TimeToFull"]
                        .downcast_ref::<i64>()
                        .expect("expected TimeToFull: i64 in HashMap of all properties");
                    let time_to_empty = raw_props["TimeToEmpty"]
                        .downcast_ref::<i64>()
                        .expect("expected TimeToEmpty: i64 in HashMap of all properties");

                    let properties = UpowerProperties {
                        percentage,
                        icon_name: icon_name.clone(),
                        state,
                        time_to_full,
                        time_to_empty,
                    };
                    properties_map.lock().await.insert(native_path, properties);
                }
                tx.send_update(properties_map.lock().await.clone()).await;
                Ok::<_, color_eyre::Report>(())
            }
        };

        spawn(init_props);

        for proxy in display_proxies.iter().cloned() {
            let proxy = proxy.clone();
            let tx = tx.clone();
            let properties_map = properties_map.clone();
            let device_interface_name = device_interface_name.clone();

            spawn(async move {
                let mut prop_changed_stream = proxy.receive_properties_changed().await?;
                let native_path = {
                    let raw_properties = proxy.get_all(device_interface_name.clone()).await?;
                    raw_properties["NativePath"]
                        .downcast_ref::<&str>()
                        .expect("expected NativePath: str in HashMap of all properties")
                        .to_string()
                };

                while let Some(signal) = prop_changed_stream.next().await {
                    let args = signal.args().expect("Invalid signal arguments");
                    if args.interface_name != device_interface_name {
                        continue;
                    }
                    let mut properties_map = properties_map.lock().await;
                    let properties = properties_map.entry(native_path.clone()).or_insert_with(|| UpowerProperties {
                        percentage: 0.0,
                        icon_name: String::new(),
                        state: BatteryState::Unknown,
                        time_to_full: 0,
                        time_to_empty: 0
                    });

                    for (name, changed_value) in args.changed_properties {
                        match name {
                            "Percentage" => {
                                properties.percentage = changed_value
                                    .downcast::<f64>()
                                    .expect("expected Percentage to be f64");
                            }
                            "IconName" => {
                                properties.icon_name = changed_value
                                    .downcast_ref::<&str>()
                                    .expect("expected IconName to be str")
                                    .to_string();
                            }
                            "State" => {
                                properties.state =
                                    u32_to_battery_state(changed_value.downcast::<u32>().unwrap_or(0))
                                        .expect("expected State to be BatteryState");
                            }
                            "TimeToFull" => {
                                properties.time_to_full = changed_value
                                    .downcast::<i64>()
                                    .expect("expected TimeToFull to be i64");
                            }
                            "TimeToEmpty" => {
                                properties.time_to_empty = changed_value
                                    .downcast::<i64>()
                                    .expect("expected TimeToEmpty to be i64");
                            }
                            _ => {}
                        }
                    }

                    tx.send_update(properties_map.clone()).await;
                }

                Result::<()>::Ok(())
            });
        }

        Ok(())
    }

    fn into_widget(
        self,
        context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        info: &ModuleInfo,
    ) -> Result<ModuleParts<Button>> {
        let icon = gtk::Image::new();
        icon.add_class("icon");

        let label = Label::builder()
            .label(&self.format)
            .use_markup(true)
            .angle(self.layout.angle(info))
            .justify(self.layout.justify.into())
            .build();

        label.add_class("label");

        let container = gtk::Box::new(self.layout.orientation(info), 5);
        container.add_class("contents");

        let button = Button::new();
        button.add_class("button");

        container.add(&icon);
        container.add(&label);
        button.add(&container);

        let tx = context.tx.clone();
        button.connect_clicked(move |button| {
            tx.send_spawn(ModuleUpdateEvent::TogglePopup(button.popup_id()));
        });

        let format = self.format.clone();

        let rx = context.subscribe();
        let provider = context.ironbar.image_provider();
        rx.recv_glib_async((), move |(), properties| {
            let state = properties["BAT0"].state;

            let is_charging =
                state == BatteryState::Charging || state == BatteryState::PendingCharge;

            let time_remaining = if is_charging {
                seconds_to_string(properties["BAT0"].time_to_full)
            } else {
                seconds_to_string(properties["BAT0"].time_to_empty)
            }
            .unwrap_or_default();

            let format = format
                .replace("{percentage}", &properties["BAT0"].percentage.round().to_string())
                .replace("{time_remaining}", &time_remaining)
                .replace("{state}", battery_state_to_string(state));

            let mut icon_name = String::from("icon:");
            icon_name.push_str(&properties["BAT0"].icon_name);

            let provider = provider.clone();
            let icon = icon.clone();

            label.set_label_escaped(&format);

            async move {
                provider
                    .load_into_image_silent(&icon_name, self.icon_size, false, &icon)
                    .await;
            }
        });

        let popup = self
            .into_popup(context, info)
            .into_popup_parts(vec![&button]);

        Ok(ModuleParts::new(button, popup))
    }

    fn into_popup(
        self,
        context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        _info: &ModuleInfo,
    ) -> Option<gtk::Box>
    where
        Self: Sized,
    {
        let container = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .build();

        let label = Label::builder().use_markup(true).build();
        label.add_class("upower-details");
        container.add(&label);

        context.subscribe().recv_glib((), move |(), properties| {
            let state = properties["BAT0"].state;
            let format = match state {
                BatteryState::Charging | BatteryState::PendingCharge => {
                    let ttf = properties["BAT0"].time_to_full;
                    if ttf > 0 {
                        format!("Full in {}", seconds_to_string(ttf).unwrap_or_default())
                    } else {
                        String::new()
                    }
                }
                BatteryState::Discharging | BatteryState::PendingDischarge => {
                    let tte = properties["BAT0"].time_to_empty;
                    if tte > 0 {
                        format!("Empty in {}", seconds_to_string(tte).unwrap_or_default())
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            };

            label.set_label_escaped(&format);
        });

        container.show_all();

        Some(container)
    }
}

fn seconds_to_string(seconds: i64) -> Result<String> {
    let mut time_string = String::new();
    let days = seconds / (DAY);
    if days > 0 {
        write!(time_string, "{days}d")?;
    }
    let hours = (seconds % DAY) / HOUR;
    if hours > 0 {
        write!(time_string, " {hours}h")?;
    }
    let minutes = (seconds % HOUR) / MINUTE;
    if minutes > 0 {
        write!(time_string, " {minutes}m")?;
    }

    Ok(time_string.trim_start().to_string())
}

const fn u32_to_battery_state(number: u32) -> Result<BatteryState, u32> {
    if number == (BatteryState::Unknown as u32) {
        Ok(BatteryState::Unknown)
    } else if number == (BatteryState::Charging as u32) {
        Ok(BatteryState::Charging)
    } else if number == (BatteryState::Discharging as u32) {
        Ok(BatteryState::Discharging)
    } else if number == (BatteryState::Empty as u32) {
        Ok(BatteryState::Empty)
    } else if number == (BatteryState::FullyCharged as u32) {
        Ok(BatteryState::FullyCharged)
    } else if number == (BatteryState::PendingCharge as u32) {
        Ok(BatteryState::PendingCharge)
    } else if number == (BatteryState::PendingDischarge as u32) {
        Ok(BatteryState::PendingDischarge)
    } else {
        Err(number)
    }
}

fn battery_state_to_string(state: BatteryState) -> &'static str {
    match state {
        BatteryState::Unknown => "Unknown",
        BatteryState::Charging => "Charging",
        BatteryState::Discharging => "Discharging",
        BatteryState::Empty => "Empty",
        BatteryState::FullyCharged => "Fully charged",
        BatteryState::PendingCharge => "Pending charge",
        BatteryState::PendingDischarge => "Pending discharge",
    }
}
