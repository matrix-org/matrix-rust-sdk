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
use futures_core::future::BoxFuture;
use matrix_sdk_crypto::CryptoStoreError;
use redis::{FromRedisValue, ToRedisArgs};

pub trait RedisConnectionShim: Send {
    fn del<'a>(&'a mut self, key: &str) -> RedisFutureShim<'a, ()>;

    fn get<'a, RV>(&'a mut self, key: &str) -> RedisFutureShim<'a, Option<RV>>
    where
        RV: FromRedisValue;

    fn set<'a, V>(&'a mut self, key: &str, value: V) -> RedisFutureShim<'a, ()>
    where
        V: ToRedisArgs + Send + Sync + 'a;

    fn hdel<'a>(&'a mut self, key: &str, field: &str) -> RedisFutureShim<'a, ()>;

    fn hgetall<'a, RV>(&'a mut self, key: &str) -> RedisFutureShim<'a, RV>
    where
        RV: FromRedisValue;

    fn hvals<'a>(&'a mut self, key: &str) -> RedisFutureShim<'a, Vec<String>>;

    fn hget<'a, RV>(&'a mut self, key: &str, field: &'a str) -> RedisFutureShim<'a, Option<RV>>
    where
        RV: FromRedisValue + Clone;

    fn hset<'a>(&'a mut self, key: &str, field: &str, value: Vec<u8>) -> RedisFutureShim<'a, ()>;

    fn sadd<'a>(&'a mut self, key: &str, value: String) -> RedisFutureShim<'a, ()>;

    fn sismember<'a>(&'a mut self, key: &str, member: &str) -> RedisFutureShim<'a, bool>;
}

#[async_trait]
pub trait RedisClientShim: Clone + Send + Sync {
    type Conn: RedisConnectionShim;

    async fn get_async_connection(&self) -> RedisResultShim<Self::Conn>;
    fn get_connection_info(&self) -> &redis::ConnectionInfo;
    fn create_pipe(&self) -> Box<dyn RedisPipelineShim<Conn = Self::Conn>>;
}

#[async_trait]
pub trait RedisPipelineShim: Send + Sync {
    type Conn: RedisConnectionShim;

    fn set(&mut self, key: &str, value: String);
    fn set_vec(&mut self, key: &str, value: Vec<u8>);
    fn del(&mut self, key: &str);
    fn hset(&mut self, key: &str, field: &str, value: Vec<u8>);
    fn hdel(&mut self, key: &str, field: &str);
    fn sadd(&mut self, key: &str, value: String);

    async fn query_async(&self, connection: &mut Self::Conn) -> RedisResultShim<()>;
}

pub type RedisResultShim<T> = Result<T, RedisErrorShim>;
pub type RedisFutureShim<'a, T> = BoxFuture<'a, RedisResultShim<T>>;

#[derive(Debug)]
pub struct RedisErrorShim {
    inner: redis::RedisError,
}

impl From<redis::RedisError> for RedisErrorShim {
    fn from(e: redis::RedisError) -> Self {
        Self { inner: e }
    }
}

impl std::fmt::Display for RedisErrorShim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

impl std::error::Error for RedisErrorShim {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.inner)
    }
}

impl From<RedisErrorShim> for CryptoStoreError {
    fn from(e: RedisErrorShim) -> Self {
        CryptoStoreError::Backend(Box::new(e))
    }
}
