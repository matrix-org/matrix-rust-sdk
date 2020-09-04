// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::io::{Cursor, Read, Seek, SeekFrom};

use base64::{decode_config, encode_config, DecodeError, STANDARD_NO_PAD};
use byteorder::{BigEndian, ReadBytesExt};

use aes_ctr::{
    stream_cipher::{NewStreamCipher, SyncStreamCipher},
    Aes256Ctr,
};
use hmac::{Hmac, Mac, NewMac};
use pbkdf2::pbkdf2;
use sha2::{Sha256, Sha512};

const SALT_SIZE: usize = 16;
const IV_SIZE: usize = 16;
const MAC_SIZE: usize = 32;
const KEY_SIZE: usize = 32;

pub fn decode(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_config(input, STANDARD_NO_PAD)
}

pub fn encode(input: impl AsRef<[u8]>) -> String {
    encode_config(input, STANDARD_NO_PAD)
}

pub fn decrypt(ciphertext: &str, passphrase: &str) -> Result<String, DecodeError> {
    let decoded = decode(ciphertext)?;

    let mut decoded = Cursor::new(decoded);

    let mut salt = [0u8; SALT_SIZE];
    let mut iv = [0u8; IV_SIZE];
    let mut mac = [0u8; MAC_SIZE];
    let mut derived_keys = [0u8; KEY_SIZE * 2];

    let version = decoded.read_u8().unwrap();
    decoded.read_exact(&mut salt).unwrap();
    decoded.read_exact(&mut iv).unwrap();

    let rounds = decoded.read_u32::<BigEndian>().unwrap();
    let ciphertext_start = decoded.position() as usize;

    decoded.seek(SeekFrom::End(-32)).unwrap();
    let ciphertext_end = decoded.position() as usize;

    decoded.read_exact(&mut mac).unwrap();

    let mut decoded = decoded.into_inner();

    if version != 1u8 {
        panic!("Unsupported version")
    }

    pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), &salt, rounds, &mut derived_keys);
    let (key, hmac_key) = derived_keys.split_at(KEY_SIZE);

    let mut hmac = Hmac::<Sha256>::new_varkey(hmac_key).unwrap();
    hmac.update(&decoded[0..ciphertext_end]);
    hmac.verify(&mac).expect("MAC DOESN'T MATCH");

    let mut ciphertext = &mut decoded[ciphertext_start..ciphertext_end];
    let mut aes = Aes256Ctr::new_var(&key, &iv).expect("Can't create AES");
    aes.apply_keystream(&mut ciphertext);

    Ok(String::from_utf8(ciphertext.to_owned()).expect("Invalid utf-8"))
}

#[cfg(test)]
mod test {
    use indoc::indoc;

    use super::{decode, decrypt};

    const PASSPHRASE: &str = "1234";

    const TEST_EXPORT: &str = indoc! {"
        -----BEGIN MEGOLM SESSION DATA-----
        AfukAbxZSTcn8SRQG/evmxHHzVGkkH7vmE2s0wWwebk+AAehIMVgIRmYAIPoxhSB4mKrsmoTbZuP4urLaWYPwCi22JhW5yvhGWp92vArL9gnN+2rB6VjDjzgR/OHkuLc
        Tv5bWKNXPubrdWQujLmzQHnSqxPmNxpEbiDBLFIjuw2gKt8m8KKZGWcOk5sdtBVA7N8pje9urQE8e+rOT7q5yx4yeydtmZC5fb38/5YOn3E8hfpSC6jpXS+3jo/0so36
        4c4l25CkkKWc07Ayhg9OjsEmDuHYuOO13r/TXojlfkaagBh3v8ZZb3+eWE4CeTV7jwVYbEy44vSKgACwLGdNX0/4TfjgfWBvOjF50h6YnprVD+vhbrG4NLg/TpdqiJ8p
        pbp6t12vUMGULQudooXGvcsCoga6p9gS+pfxn9yhONBPU+pBFo+1Fnq80ZN8ErjVv58n3hLpH7YbvFwBPLBAj2h7dCHtj92E14jSgUvg+vRAAI5TwcZfuQwzH5qL6+Zh
        1+pht9RsqplnbbdR32M3lypncAWgsUYtR/4wEBwlTSYCFW3GSm/Ow9XaWKF5RCZf9UTlOJ5veSkDZW61GCVLgsZj06Y+sji7IN7kGOBv4SfkYbhloLm0xPFCuQTbijqh
        OCdzwH/lApCZflqBHLwAWsPuLrhgax99xs/QN9MIx8hh/pRc0pNNrBOgF1SJWQ/ChAuB7KbHcf4k1IFM0I4XH6u2GKpOxsQtulTulX3Sm+gCo0JBTO1DdYZPe7x0Enhl
        dYojj6op4+HpJUw6Alh2g4SeCmZRcaux+0hwSCkuPRBX4fwgv+Qj0abgqATaLTGI6VXAP3ya4thaNNfEurj3+20b39VL6Bz8mW5g8aWo9DZ7/Ph6t042613wz9rKKrFv
        ozKEqUX9Rbs01IknK2e8iakUKvzQjZ0ezs+XBy/0OKfyc+0KIj4vFvoTL9TePmOdBNNFQz91s21YDziE3fO8jJQLZGgf91ttCdtoouT2RAM1J7uIw0+4Ar0ytE2GiUgO
        w/Zbo7M8CifIJ6CVM6F1dVdX7hj61lsbhzIV46xhscH6KszGioWD8zUaaXwt5b3Hrvsy4YbIRLhkVq1sZR/ETC+dqbSa/1cYngNwkoVspksK3JAaK1xVWGHFboXnxOtN
        S+iWtIfWOIy9ExfA9+bz4y1s/VkruQAIIJJg/KH2bCsGMM+FHO3OaMOf4bvdnGH0SKpAk0WqVVZME6rTw5XqObqi5AF7Y+m8wyK87zdn3+n6JPBb7ROh/m4kuOcNvs9A
        Gkhi8K4Q+/Ymeq6NNjkuvdKrlJxzdiwggCTA941158xnrs470QC0R/xDKfoZeFLWW6GhbwO6AuRy1vg4lkidrrD/zrczTMtOajhqL6CGARiufdbmByNVo1sybwTT/jOh
        /divg/jY1RNY+IbEjOOWKnVcxrYURUFWjur4BgwkGBy9RovZRK6Co0gCXGwsdFrR9LG7jnGXiXm2fjGYDQghj8opC3wVtC7O0DQVrjDGRTZPOjoIgzvRomt7wbFosrww
        Bw5RxiL5bA7bI9h1KuIPfR4+C2LjuRdnNSX0s+iQkz1cbwfGsa2pN1unzPwC0Gul6tciu2A32akfDQVzJrSCDahnoQXVZEEk0TdPY2oIZsEOmWZf94GQKr1IR7vJ37xj
        iaYFKid+9F+ELlk2i6DQYDzXMacg55mWLuQkxC8MR5L9mPTtwoeVjbpbdzYctKdJ9KqeZTSh0qaSRnzuy3F7HnifwhkKI5JjyBQ5734gUmrikAfvtcBcpcd3ctX3+cwN
        VwFxyQcwkTWNt4AQnzNVM3ZCxLEyMGqZ7XR3FQoxw8xTXJ4tSclI/g6vzKMVjHmuPRSMQj8PLhpICRnk5k50UmwS+rZTVqnHeJ+XF6JrEOYGgfOIVUUG5N+NCiBgvQKr
        xa0ImLQXzJIdJqB2CLWIWytaqGSY4+Px2jSxQCIhl4PSBs5Ia1iBgZ3578Dp0DYz0caEzggDurLHrth1kVzC3zqm/D+uPBhF3iMOyp7gHuMOsCY1LKB2eyiizz1Hrhb4
        ZBGGo15dC0KMP9uhmDeaijoH7YO5wReD427KNf5Z4kIFrgP0ebGr3sObZm74yJBlfnR0kKhFsQN7b/WthZIna7F/zIQaPYncitJqMTIgHXptrhaCV+c7AARs1pw/cnOQ
        bH5evUA2zibgqaAOselrgnxbIqG2LQk84184b7V4Lg0+ebAjtTEVGqAiVVhydA7OCXETWfrS5le0W1/DAtv80K6KBMCkS33rKHUdTPru2fNGmo+angp9lO/ANQ==
        -----END MEGOLM SESSION DATA-----
    "};

    fn export_wihtout_headers() -> String {
        TEST_EXPORT
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect()
    }

    #[test]
    fn test_decode() {
        let export = export_wihtout_headers();
        assert!(decode(export).is_ok());
    }

    #[test]
    fn test_decrypt() {
        let export = export_wihtout_headers();
        let decrypted = decrypt(&export, PASSPHRASE).expect("Can't decrypt key export");
    }
}
