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
    subscriptions: Mutex<HashMap<&'static str, broadcast::Sender<Publish>>>,
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
                            subscriptions.get(&topic).cloned()
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
    async fn subscribe(&self, topic: &'static str) -> Result<broadcast::Receiver<Publish>> {
        let (receiver, needs_subscription) = {
            let mut subscriptions = self.subscriptions.lock().unwrap();
            match subscriptions.entry(topic) {
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let server = Server::new();

    task::spawn({
        let server = server.clone();
        async move {
            let mut subscription = server.subscribe("tacotorch/test/set").await.unwrap();
            loop {
                let message = subscription.recv().await.unwrap();
                server
                    .publish("zigbee2mqtt/Büro Accent 1/set", message.payload)
                    .await
                    .unwrap();
            }
        }
    });

    task::spawn({
        let server = server.clone();
        async move {
            let mut subscription = server
                .subscribe("zigbee2mqtt/Büro Accent 1/set")
                .await
                .unwrap();
            loop {
                let message = subscription.recv().await.unwrap();
                server
                    .publish("tacotorch/test", message.payload)
                    .await
                    .unwrap();
            }
        }
    });

    let light_config = serde_json::to_vec(&json!({
        "name": null,
        "availability": [
            {"topic":"tacotorch/server/state","value_template":"{{ value_json.state }}"},
            {"topic":"zigbee2mqtt/bridge/state","value_template":"{{ value_json.state }}"},
            {"topic":"zigbee2mqtt/Büro Accent 1/availability","value_template":"{{ value_json.state }}"}
        ],
        "availability_mode": "all",
        "brightness": true,
        "state_topic": "tacotorch/test",
        "command_topic": "tacotorch/test/set",
        "json_attributes_topic":"tacotorch/test",
        "schema": "json",
        "device": {
            "identifiers": ["test4"],
            "name": "test",
            "manufacturer": "me",
            "model": "Some Model",
        },
        "supported_color_modes":["rgb"],
        "unique_id": "tacotorch_light_test",
    }))?;

    server
        .mqtt
        .publish(
            "homeassistant/light/tacotorch_light_test/config",
            QoS::AtLeastOnce,
            false,
            light_config,
        )
        .await?;
    loop {
        tokio::time::sleep(Duration::from_secs(u64::MAX)).await;
    }
}
