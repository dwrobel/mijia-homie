use blurz::{
    BluetoothAdapter, BluetoothDevice, BluetoothDiscoverySession, BluetoothEvent, BluetoothSession,
};
use futures::FutureExt;
use homie::{Datatype, HomieDevice, Node, Property};
use mijia::{
    decode_value, find_sensors, hashmap_from_file, print_sensors, start_notify_sensor,
    SERVICE_CHARACTERISTIC_PATH,
};
use rumqttc::MqttOptions;
use rustls::ClientConfig;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::{task, time, try_join};

const DEFAULT_MQTT_PREFIX: &str = "homie";
const DEFAULT_DEVICE_ID: &str = "mijia-bridge";
const DEFAULT_DEVICE_NAME: &str = "Mijia bridge";
const DEFAULT_HOST: &str = "test.mosquitto.org";
const DEFAULT_PORT: u16 = 1883;
const SCAN_DURATION: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT_MS: i32 = 4_000;
const INCOMING_TIMEOUT_MS: u32 = 1_000;
const SENSOR_NAMES_FILENAME: &str = "sensor_names.conf";

async fn scan(bt_session: &BluetoothSession) -> Result<Vec<String>, Box<dyn Error>> {
    let adapter: BluetoothAdapter = BluetoothAdapter::init(bt_session)?;
    adapter.set_powered(true)?;

    let discover_session =
        BluetoothDiscoverySession::create_session(&bt_session, adapter.get_id())?;
    discover_session.start_discovery()?;
    println!("Scanning");
    // Wait for the adapter to scan for a while.
    time::delay_for(SCAN_DURATION).await;
    let device_list = adapter.get_device_list()?;

    discover_session.stop_discovery()?;

    println!("{:?} devices found", device_list.len());

    Ok(device_list)
}

#[tokio::main(core_threads = 2)]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    dotenv::dotenv()?;
    pretty_env_logger::init();
    color_backtrace::install();

    let device_id = std::env::var("DEVICE_ID").unwrap_or_else(|_| DEFAULT_DEVICE_ID.to_string());
    let device_name =
        std::env::var("DEVICE_NAME").unwrap_or_else(|_| DEFAULT_DEVICE_NAME.to_string());
    let client_name = std::env::var("CLIENT_NAME").unwrap_or_else(|_| device_id.clone());

    let host = std::env::var("HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());

    let port = std::env::var("PORT")
        .ok()
        .and_then(|val| val.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);

    let mut mqttoptions = MqttOptions::new(client_name, host, port);

    let username = std::env::var("USERNAME").ok();
    let password = std::env::var("PASSWORD").ok();

    mqttoptions.set_keep_alive(5);
    if let (Some(u), Some(p)) = (username, password) {
        mqttoptions.set_credentials(u, p);
    }

    // Use `env -u USE_TLS` to unset this variable if you need to clear it.
    if std::env::var("USE_TLS").is_ok() {
        let mut client_config = ClientConfig::new();
        client_config.root_store =
            rustls_native_certs::load_native_certs().expect("could not load platform certs");
        mqttoptions.set_tls_client_config(Arc::new(client_config));
    }

    let mqtt_prefix =
        std::env::var("MQTT_PREFIX").unwrap_or_else(|_| DEFAULT_MQTT_PREFIX.to_string());
    let device_base = format!("{}/{}", mqtt_prefix, device_id);
    let (homie, mqtt_handle) = HomieDevice::builder(&device_base, &device_name, mqttoptions)
        .spawn()
        .await?;

    let local = task::LocalSet::new();

    let bluetooth_handle = local.spawn_local(async move {
        requests(homie).await.unwrap();
    });

    // Poll everything to completion, until the first one bombs out.
    let res: Result<_, Box<dyn Error + Send + Sync>> = try_join! {
        // LocalSet finished first. Colour me confused.
        local.map(|()| Ok(println!("WTF?"))),
        // Bluetooth finished first. Convert error and get on with your life.
        bluetooth_handle.map(|res| Ok(res?)),
        // MQTT event loop finished first.
        // Unwrap the JoinHandle Result to get to the real Result.
        mqtt_handle,
    };
    res?;
    Ok(())
}

fn node_id_name_for_sensor(
    sensor: &BluetoothDevice,
    sensor_names: &HashMap<String, String>,
) -> Result<(String, String), Box<dyn Error>> {
    let mac_address = sensor.get_address()?;
    let node_id = mac_address.replace(":", "");
    let node_name = sensor_names
        .get(&mac_address)
        .cloned()
        .unwrap_or(mac_address);
    Ok((node_id, node_name))
}

async fn connect_start_sensor<'a>(
    bt_session: &'a BluetoothSession,
    homie: &mut HomieDevice,
    sensor_names: &HashMap<String, String>,
    properties: Vec<Property>,
    sensor: &BluetoothDevice<'a>,
) -> Result<(), Box<dyn Error>> {
    sensor.connect(CONNECT_TIMEOUT_MS)?;
    start_notify_sensor(bt_session, &sensor)?;
    let (node_id, node_name) = node_id_name_for_sensor(sensor, sensor_names)?;
    homie
        .add_node(Node::new(
            node_id,
            node_name,
            "Mijia sensor".to_string(),
            properties.to_vec(),
        ))
        .await?;
    Ok(())
}

async fn requests(mut homie: HomieDevice) -> Result<(), Box<dyn Error>> {
    let sensor_names = hashmap_from_file(SENSOR_NAMES_FILENAME)?;

    let bt_session = &BluetoothSession::create_session(None)?;
    let device_list = scan(&bt_session).await?;
    let sensors = find_sensors(&bt_session, &device_list);
    print_sensors(&sensors, &sensor_names);
    let (named_sensors, unnamed_sensors): (Vec<_>, Vec<_>) = sensors
        .into_iter()
        .partition(|sensor| sensor_names.contains_key(&sensor.get_address().unwrap()));
    println!(
        "{} named sensors, {} unnamed sensors",
        named_sensors.len(),
        unnamed_sensors.len()
    );

    let properties = [
        Property::new("temperature", "Temperature", Datatype::Float, Some("ºC")),
        Property::new("humidity", "Humidity", Datatype::Integer, Some("%")),
        Property::new("battery", "Battery level", Datatype::Integer, Some("%")),
    ];

    let mut sensors_to_connect: VecDeque<_> = named_sensors.into();

    homie.ready().await?;

    let mut sensors_connected: Vec<BluetoothDevice> = vec![];

    loop {
        println!("{} sensors in queue to connect.", sensors_to_connect.len());
        // Try to connect to a sensor.
        if let Some(sensor) = sensors_to_connect.pop_front() {
            let mac_address = sensor.get_address()?;
            let name = sensor_names.get(&mac_address).unwrap_or(&mac_address);
            println!("Trying to connect to {}", name);
            match connect_start_sensor(
                bt_session,
                &mut homie,
                &sensor_names,
                properties.to_vec(),
                &sensor,
            )
            .await
            {
                Err(e) => {
                    println!("Failed to connect to {}: {:?}", name, e);
                    sensors_to_connect.push_back(sensor);
                }
                Ok(()) => {
                    println!("Connected to {} and started notifications", name);
                    sensors_connected.push(sensor);
                }
            }
        }

        // Process events until there are none available for the timeout.
        for event in bt_session
            .incoming(INCOMING_TIMEOUT_MS)
            .map(BluetoothEvent::from)
        {
            match event {
                Some(BluetoothEvent::Value { object_path, value }) => {
                    // TODO: Make this less hacky.
                    let device_path = match object_path.strip_suffix(SERVICE_CHARACTERISTIC_PATH) {
                        Some(path) => path,
                        None => continue,
                    };
                    let device = BluetoothDevice::new(bt_session, device_path.to_string());

                    if let Some(readings) = decode_value(&value) {
                        let (node_id, name) = node_id_name_for_sensor(&device, &sensor_names)?;
                        println!("{} {} ({})", device.get_id(), readings, name);

                        homie
                            .publish_value(
                                &node_id,
                                "temperature",
                                format!("{:.2}", readings.temperature),
                            )
                            .await?;
                        homie
                            .publish_value(&node_id, "humidity", readings.humidity)
                            .await?;
                        homie
                            .publish_value(&node_id, "battery", readings.battery_percent)
                            .await?;
                    } else {
                        println!("Invalid value from {}", device.get_id());
                    }
                }
                Some(BluetoothEvent::Connected {
                    object_path,
                    connected: false,
                }) => {
                    if let Some(sensor_index) = sensors_connected
                        .iter()
                        .position(|s| s.get_id() == object_path)
                    {
                        let sensor = sensors_connected.remove(sensor_index);
                        let (node_id, node_name) = node_id_name_for_sensor(&sensor, &sensor_names)?;
                        println!("{} disconnected", node_name);
                        homie.remove_node(&node_id).await?;
                        sensors_to_connect.push_back(sensor);
                    } else {
                        println!(
                            "{} disconnected but wasn't known to be connected.",
                            object_path
                        );
                    }
                    continue;
                }
                _ => {
                    log::trace!("{:?}", event);
                    continue;
                }
            };
        }
    }
}
