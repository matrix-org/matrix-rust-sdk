// Copyright 2022 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use async_trait::async_trait;
#[cfg(feature = "crypto-store")]
use redis::RedisConnectionInfo;
use redis::{FromRedisValue, ToRedisArgs};

use crate::redis_shim::{
    RedisClientShim, RedisConnectionShim, RedisFutureShim, RedisPipelineShim, RedisResultShim,
};

#[derive(Clone)]
pub struct FakeRedisConnection {
    values: Arc<std::sync::Mutex<HashMap<String, redis::Value>>>,
}

impl RedisConnectionShim for FakeRedisConnection {
    fn del<'a>(&'a mut self, key: &str) -> RedisFutureShim<'a, ()> {
        self.values.lock().unwrap().remove(key);
        Box::pin(async move { Ok(()) })
    }

    fn get<'a, RV>(&'a mut self, key: &str) -> RedisFutureShim<'a, Option<RV>>
    where
        RV: FromRedisValue,
    {
        let ret = self.values.lock().unwrap().get(key).cloned();
        Box::pin(async move {
            match ret {
                None => Ok(None),
                Some(r) => RV::from_redis_value(&r).map(|v| Some(v)),
            }
            .map_err(|e| e.into())
        })
    }

    fn set<'a, V>(&'a mut self, key: &str, value: V) -> RedisFutureShim<'a, ()>
    where
        V: ToRedisArgs + Send + Sync + 'a,
    {
        let vs = value.to_redis_args();
        assert_eq!(vs.len(), 1);
        let v: redis::Value = redis::Value::Data(vs[0].clone());
        self.values.lock().unwrap().insert(String::from(key), v);
        Box::pin(async move { Ok(()) })
    }

    fn hgetall<'a, RV>(&'a mut self, key: &str) -> RedisFutureShim<'a, RV>
    where
        RV: FromRedisValue,
    {
        let ret = self
            .values
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_else(|| redis::Value::Bulk(Vec::new()));
        Box::pin(async move { RV::from_redis_value(&ret).map_err(|e| e.into()) })
    }

    fn hdel<'a>(&'a mut self, key: &str, field: &str) -> RedisFutureShim<'a, ()> {
        let mut values = self.values.lock().unwrap();
        let entry = values
            .entry(String::from(key))
            .or_insert(to_redis_value(BTreeMap::<String, Vec<u8>>::new()));
        let mut full_map: BTreeMap<String, Vec<u8>> = BTreeMap::from_redis_value(entry)
            .unwrap_or_else(|_| {
                panic!("Tried to hdel {key} as a btreemap, but it is not a btreemap!")
            });
        full_map.remove(field);

        // Replace the entry at key with this modified hashmap
        *entry = to_redis_value(full_map);

        // Return a future
        Box::pin(async move { Ok(()) })
    }

    fn hget<'a, RV>(&'a mut self, key: &str, field: &str) -> RedisFutureShim<'a, Option<RV>>
    where
        RV: FromRedisValue + Clone,
    {
        let value = self
            .values
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_else(|| redis::Value::Bulk(Vec::new()));

        let field = String::from(field);

        Box::pin(async move {
            BTreeMap::<String, RV>::from_redis_value(&value)
                .map(|hm| hm.get(&field).cloned())
                .map_err(|e| e.into())
        })
    }

    fn hset<'a>(&'a mut self, key: &str, field: &str, value: Vec<u8>) -> RedisFutureShim<'a, ()> {
        let mut values = self.values.lock().unwrap();
        let entry = values
            .entry(String::from(key))
            .or_insert(to_redis_value(BTreeMap::<String, Vec<u8>>::new()));
        let mut full_map: BTreeMap<String, Vec<u8>> = BTreeMap::from_redis_value(entry)
            .unwrap_or_else(|_| {
                panic!("Tried to hset {key} as a btreemap, but it is not a btreemap!")
            });
        full_map.insert(String::from(field), value);

        // Replace the entry at key with this modified hashmap
        *entry = to_redis_value(full_map);

        // Return a future
        Box::pin(async move { Ok(()) })
    }

    fn hvals<'a>(&'a mut self, key: &str) -> RedisFutureShim<'a, Vec<String>> {
        let value = self
            .values
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_else(|| redis::Value::Bulk(Vec::new()));

        Box::pin(async move {
            BTreeMap::<String, String>::from_redis_value(&value)
                .map(|hm| hm.values().cloned().collect())
                .map_err(|e| e.into())
        })
    }

    fn sadd<'a>(&'a mut self, key: &str, value: String) -> RedisFutureShim<'a, ()> {
        let mut values = self.values.lock().unwrap();
        let entry =
            values.entry(String::from(key)).or_insert(to_redis_value(BTreeSet::<String>::new()));
        let mut full_map: BTreeSet<String> =
            BTreeSet::from_redis_value(entry).unwrap_or_else(|_| {
                panic!("Tried to sadd {key} as a btreeset, but it is not a btreeset!")
            });
        full_map.insert(value);

        // Replace the entry at key with this modified hashmap
        *entry = to_redis_value(full_map);

        // Return a future
        Box::pin(async move { Ok(()) })
    }

    fn sismember<'a>(&'a mut self, key: &str, member: &str) -> RedisFutureShim<'a, bool> {
        let value = self
            .values
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_else(|| redis::Value::Bulk(Vec::new()));

        let member = String::from(member);

        Box::pin(async move {
            BTreeSet::<String>::from_redis_value(&value)
                .map(|se| se.contains(&member))
                .map_err(|e| e.into())
        })
    }
}

fn to_redis_value<T>(obj: T) -> redis::Value
where
    T: ToRedisArgs,
{
    let bytes_vec = obj.to_redis_args();
    let bytes: Vec<redis::Value> =
        bytes_vec.iter().map(|item| redis::Value::Data(item.clone())).collect();
    redis::Value::Bulk(bytes)
}

#[derive(Clone)]
pub struct FakeRedisClient {
    connection_info: redis::ConnectionInfo,
    values: Arc<std::sync::Mutex<HashMap<String, redis::Value>>>,
}

impl FakeRedisClient {
    #[cfg(feature = "crypto-store")]
    pub fn new() -> Self {
        Self {
            connection_info: redis::ConnectionInfo {
                addr: redis::ConnectionAddr::Tcp(String::from(""), 0),
                redis: RedisConnectionInfo { db: 0, username: None, password: None },
            },
            values: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl RedisClientShim for FakeRedisClient {
    type Conn = FakeRedisConnection;

    async fn get_async_connection(&self) -> RedisResultShim<Self::Conn> {
        Ok(FakeRedisConnection { values: self.values.clone() })
    }

    fn get_connection_info(&self) -> &redis::ConnectionInfo {
        &self.connection_info
    }

    fn create_pipe(&self) -> Box<dyn RedisPipelineShim<Conn = Self::Conn>> {
        Box::new(FakeRedisPipeline::new())
    }
}

enum PipelineCommand {
    Del(String),
    Hdel(String, String),
    Hset(String, String, Vec<u8>),
    Sadd(String, String),
    Set(String, String),
    SetVec(String, Vec<u8>),
}

struct FakeRedisPipeline {
    cmds: Vec<PipelineCommand>,
}

impl FakeRedisPipeline {
    pub fn new() -> Self {
        Self { cmds: Vec::new() }
    }
}

#[async_trait]
impl RedisPipelineShim for FakeRedisPipeline {
    type Conn = FakeRedisConnection;

    fn set(&mut self, key: &str, value: String) {
        self.cmds.push(PipelineCommand::Set(String::from(key), value));
    }

    fn set_vec(&mut self, key: &str, value: Vec<u8>) {
        self.cmds.push(PipelineCommand::SetVec(String::from(key), value));
    }

    fn del(&mut self, key: &str) {
        self.cmds.push(PipelineCommand::Del(String::from(key)));
    }

    fn hset(&mut self, key: &str, field: &str, value: Vec<u8>) {
        self.cmds.push(PipelineCommand::Hset(String::from(key), String::from(field), value));
    }

    fn hdel(&mut self, key: &str, field: &str) {
        self.cmds.push(PipelineCommand::Hdel(String::from(key), String::from(field)));
    }

    fn sadd(&mut self, key: &str, value: String) {
        self.cmds.push(PipelineCommand::Sadd(String::from(key), value));
    }

    async fn query_async(&self, connection: &mut Self::Conn) -> RedisResultShim<()> {
        for cmd in &self.cmds {
            match cmd {
                PipelineCommand::Del(key) => connection.del(key).await?,
                PipelineCommand::Hdel(key, field) => connection.hdel(key, field).await?,
                PipelineCommand::Hset(key, field, value) => {
                    connection.hset(key, field, value.clone()).await?
                }
                PipelineCommand::Sadd(key, value) => connection.sadd(key, value.to_owned()).await?,
                PipelineCommand::Set(key, value) => connection.set(key, value).await?,
                PipelineCommand::SetVec(key, value) => connection.set(key, value).await?,
            }
        }
        Ok(())
    }
}
