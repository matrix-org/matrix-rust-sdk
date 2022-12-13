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

use async_trait::async_trait;
use redis::{AsyncCommands, FromRedisValue, ToRedisArgs};

use crate::redis_shim::{
    RedisClientShim, RedisConnectionShim, RedisFutureShim, RedisPipelineShim, RedisResultShim,
};

impl RedisConnectionShim for redis::aio::Connection {
    fn del<'a>(&'a mut self, key: &str) -> RedisFutureShim<'a, ()> {
        let key = key.to_owned();
        Box::pin(async move { AsyncCommands::del(self, key).await.map_err(|e| e.into()) })
    }

    fn get<'a, RV>(&'a mut self, key: &str) -> RedisFutureShim<'a, Option<RV>>
    where
        RV: FromRedisValue,
    {
        let key = key.to_owned();

        Box::pin(async move { AsyncCommands::get(self, key).await.map_err(|e| e.into()) })
    }

    fn set<'a, V>(&'a mut self, key: &str, value: V) -> RedisFutureShim<'a, ()>
    where
        V: ToRedisArgs + Send + Sync + 'a,
    {
        AsyncCommands::set::<&str, V, ()>(self, key, value);
        Box::pin(async move { Ok(()) })
    }

    fn hdel<'a>(&'a mut self, key: &str, field: &str) -> RedisFutureShim<'a, ()> {
        let key = key.to_owned();
        let field = field.to_owned();

        Box::pin(async move { AsyncCommands::hdel(self, key, field).await.map_err(|e| e.into()) })
    }

    fn hgetall<'a, RV>(&'a mut self, key: &str) -> RedisFutureShim<'a, RV>
    where
        RV: FromRedisValue,
    {
        let key = key.to_owned();
        Box::pin(async move { AsyncCommands::hgetall(self, key).await.map_err(|e| e.into()) })
    }

    fn hvals<'a>(&'a mut self, key: &str) -> RedisFutureShim<'a, Vec<String>> {
        let key = key.to_owned();
        Box::pin(async move { AsyncCommands::hvals(self, key).await.map_err(|e| e.into()) })
    }

    fn hget<'a, RV>(&'a mut self, key: &str, field: &'a str) -> RedisFutureShim<'a, Option<RV>>
    where
        RV: FromRedisValue + Clone,
    {
        let key = key.to_owned();
        let field = field.to_owned();

        Box::pin(async move { AsyncCommands::hget(self, key, field).await.map_err(|e| e.into()) })
    }

    fn hset<'a>(&'a mut self, key: &str, field: &str, value: Vec<u8>) -> RedisFutureShim<'a, ()> {
        let key = key.to_owned();
        let field = field.to_owned();

        Box::pin(async move {
            AsyncCommands::hset::<_, _, _, ()>(self, key, field, value).await.map_err(|e| e.into())
        })
    }

    fn sadd<'a>(&'a mut self, key: &str, value: String) -> RedisFutureShim<'a, ()> {
        let key = key.to_owned();

        Box::pin(async move { AsyncCommands::sadd(self, key, value).await.map_err(|e| e.into()) })
    }

    fn sismember<'a>(&'a mut self, key: &str, member: &str) -> RedisFutureShim<'a, bool> {
        let key = key.to_owned();
        let member = member.to_owned();

        Box::pin(
            async move { AsyncCommands::sismember(self, key, member).await.map_err(|e| e.into()) },
        )
    }
}

#[async_trait]
impl RedisClientShim for redis::Client {
    type Conn = redis::aio::Connection;

    async fn get_async_connection(&self) -> RedisResultShim<Self::Conn> {
        self.client.get_async_connection().await.map_err(|e| e.into())
    }

    fn get_connection_info(&self) -> &redis::ConnectionInfo {
        self.client.get_connection_info()
    }

    fn create_pipe(&self) -> Box<dyn RedisPipelineShim<Conn = Self::Conn>> {
        let mut pipeline = redis::pipe();
        pipeline.atomic();
        Box::new(pipeline)
    }
}

#[async_trait]
impl RedisPipelineShim for redis::Pipeline {
    type Conn = redis::aio::Connection;

    fn set(&mut self, key: &str, value: String) {
        self.pipeline.set(key, value);
    }

    fn set_vec(&mut self, key: &str, value: Vec<u8>) {
        self.pipeline.set(key, value);
    }

    fn del(&mut self, key: &str) {
        self.pipeline.del(key);
    }

    fn hset(&mut self, key: &str, field: &str, value: Vec<u8>) {
        self.pipeline.hset(key, field, value);
    }

    fn hdel(&mut self, key: &str, field: &str) {
        self.pipeline.hdel(key, field);
    }

    fn sadd(&mut self, key: &str, value: String) {
        self.pipeline.sadd(key, value);
    }

    async fn query_async(&self, connection: &mut Self::Conn) -> RedisResultShim<()> {
        self.pipeline.query_async(connection).await.map_err(|e| e.into())
    }
}
