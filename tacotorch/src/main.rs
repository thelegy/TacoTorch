#![feature(never_type)]

use anyhow::{anyhow, Result};
use core::panic;
use rumqttc::{
    ConnectReturnCode, Event, LastWill, MqttOptions, Packet, Publish, QoS, SubscribeFilter,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::broadcast, task};

pub struct Server {
    subscriptions: Mutex<HashMap<String, broadcast::Sender<Publish>>>,
    pub mqtt: rumqttc::AsyncClient,
}
impl Server {
    fn new() -> Arc<Self> {
        let mut mqtt_options = MqttOptions::new("tacotorch", "192.168.1.19", 1883);
        mqtt_options.set_keep_alive(Duration::from_secs(5));
        mqtt_options.set_max_packet_size(10000000, 20000);
        mqtt_options.set_last_will(LastWill::new(
            "tacotorch/server/state",
            "{\"state\":\"offline\"}",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, event_loop) = rumqttc::AsyncClient::new(mqtt_options, 10);

        let server = Arc::new(Self {
            subscriptions: Default::default(),
            mqtt: client,
        });

        task::spawn(server.clone().event_loop(event_loop));

        server
    }
    async fn event_loop(self: Arc<Self>, mut event_loop: rumqttc::EventLoop) -> ! {
        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(incoming)) => match incoming {
                    Packet::Publish(message) => {
                        let sender_option = {
                            let subscriptions = self.subscriptions.lock().unwrap();
                            let topic: &str = &message.topic;
                            subscriptions.get(topic).cloned()
                        };
                        if let Some(sender) = sender_option {
                            let _ = sender.send(message);
                        }
                    }
                    Packet::PingResp => {}
                    Packet::SubAck(_) => {}
                    Packet::PubAck(_) => {}
                    Packet::ConnAck(ack) => {
                        if ack.code != ConnectReturnCode::Success {
                            panic!("Connection failed with code: {:?}", ack.code);
                        }
                        self.mqtt
                            .publish(
                                "tacotorch/server/state",
                                QoS::AtLeastOnce,
                                true,
                                "{\"state\":\"online\"}",
                            )
                            .await
                            .unwrap();
                        let mut filters = Vec::new();
                        {
                            let subscriptions = self.subscriptions.lock().unwrap();
                            for topic in subscriptions.keys() {
                                filters.push(SubscribeFilter::new(
                                    topic.to_string(),
                                    QoS::AtLeastOnce,
                                ));
                            }
                        }
                        if !filters.is_empty() {
                            self.mqtt.subscribe_many(filters).await.unwrap();
                        }
                    }
                    _ => {
                        println!("Received other = {:?}", incoming);
                    }
                },
                Ok(Event::Outgoing(_)) => {}
                Err(err) => println!("EventLoop: Err: {:?}", err),
            }
        }
    }
    async fn publish<S, V>(&self, topic: S, payload: V) -> Result<()>
    where
        S: Into<String>,
        V: Into<Vec<u8>>,
    {
        self.mqtt
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await?;
        Ok(())
    }
    async fn subscribe<S>(&self, topic: S) -> Result<broadcast::Receiver<Publish>>
    where
        S: Into<String> + Clone,
    {
        let (receiver, needs_subscription) = {
            let mut subscriptions = self.subscriptions.lock().unwrap();
            match subscriptions.entry(topic.clone().into()) {
                std::collections::hash_map::Entry::Occupied(x) => (x.get().subscribe(), false),
                std::collections::hash_map::Entry::Vacant(x) => {
                    let (sender, receiver) = broadcast::channel(3);
                    x.insert(sender);
                    (receiver, true)
                }
            }
        };
        if needs_subscription {
            self.mqtt.subscribe(topic, QoS::AtLeastOnce).await?;
        }
        Ok(receiver)
    }
    async fn transition(
        &self,
        lamp: &str,
        seconds: f32,
        mut value: serde_json::Value,
    ) -> Result<()> {
        if let Some(object) = value.as_object_mut() {
            object.insert("state".into(), "ON".into());
            object.insert("transition".into(), seconds.into());
            self.publish(
                "zigbee2mqtt/".to_owned() + lamp + "/set",
                serde_json::to_vec(object)?,
            )
            .await?;
            tokio::time::sleep(Duration::from_secs_f32(seconds)).await;
            Ok(())
        } else {
            Err(anyhow!("Bad value for color transition"))
        }
    }
    pub fn add_lamp<S1, S2>(self: &Arc<Self>, name: S1, z2m_prefix: S2) -> Result<Arc<Lamp>>
    where
        S1: Into<String>,
        S2: Into<String>,
    {
        let lamp = Arc::new(Lamp {
            server: self.clone(),
            name: name.into(),
            z2m_prefix: z2m_prefix.into(),
        });
        task::spawn({
            let lamp = lamp.clone();
            async move {
                // TODO handle errors
                let _ = tokio::join!(lamp.handle_set(), lamp.handle_lamp_state());
            }
        });
        Ok(lamp)
    }
}

pub struct Lamp {
    server: Arc<Server>,
    name: String,
    z2m_prefix: String,
}
impl Lamp {
    async fn set(&self, payload: &[u8]) -> Result<()> {
        self.server
            .publish(format!("zigbee2mqtt/{}/set", self.z2m_prefix), payload)
            .await
    }
    async fn handle_set(&self) -> Result<!> {
        let mut subscription = self
            .server
            .subscribe(format!("tacotorch/{}/set", self.name).to_owned())
            .await?;
        loop {
            let message = subscription.recv().await.unwrap();
            self.server
                .publish(
                    format!("zigbee2mqtt/{}/set", self.z2m_prefix),
                    message.payload,
                )
                .await?;
        }
    }
    async fn handle_lamp_state(&self) -> Result<!> {
        let mut subscription = self
            .server
            .subscribe(format!("zigbee2mqtt/{}", self.z2m_prefix))
            .await?;
        loop {
            let message = subscription.recv().await.unwrap();
            self.server
                .publish(format!("tacotorch/{}", self.name), message.payload)
                .await?;
        }
    }
    async fn advertise_ha(&self) -> Result<()> {
        let availability = json!([
            {"topic":"tacotorch/server/state","value_template":"{{ value_json.state }}"},
            {"topic":"zigbee2mqtt/bridge/state","value_template":"{{ value_json.state }}"},
            {
                "topic": format!("zigbee2mqtt/{}/availability", self.z2m_prefix),
                "value_template": "{{ value_json.state }}",
            }
        ]);

        let id = format!("tacotorch_light_{}", self.name);

        let device = json!({
            "identifiers": [id],
            "name": self.name,
        });

        let light_config = serde_json::to_vec(&json!({
            "name": null,
            "availability": availability,
            "availability_mode": "all",
            "brightness": true,
            "state_topic": format!("tacotorch/{}", self.name),
            "json_attributes_topic": format!("tacotorch/{}", self.name),
            "command_topic": format!("tacotorch/{}/set", self.name),
            "schema": "json",
            "device": device,
            "supported_color_modes": ["rgb"],
            "unique_id": id,
        }))?;

        self.server
            .publish(format!("homeassistant/light/{}/config", id), light_config)
            .await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let server = Server::new();

    let lamp_test = server.add_lamp("test", "Büro Accent 1")?;

    lamp_test.advertise_ha().await?;

    loop {
        tokio::time::sleep(Duration::from_secs(u64::MAX)).await;
    }
}
