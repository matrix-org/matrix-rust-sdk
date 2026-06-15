/*
Copyright 2026 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use rustls::SignatureScheme;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An X.509 signature and certificate chain
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct X509Signature {
    /// The PEM-encoded certificate chain, starting with the device's own
    /// certificate, followed by intermediate certificates.
    pub certificate_chain: String,

    /// The scheme used for this signature.
    #[serde(
        serialize_with = "signature_scheme_to_u16",
        deserialize_with = "u16_to_signature_scheme"
    )]
    pub signature_scheme: SignatureScheme,

    /// The base64-encoded signature itself
    pub signature: String,
}

/// Serialize a SignatureScheme by treating it as a u16
fn signature_scheme_to_u16<S>(
    signature_scheme: &SignatureScheme,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_u16(u16::from(*signature_scheme))
}

/// Deserialize a SignatureScheme from a u16
fn u16_to_signature_scheme<'de, D>(deserializer: D) -> Result<SignatureScheme, D::Error>
where
    D: Deserializer<'de>,
{
    u16::deserialize(deserializer).map(SignatureScheme::from)
}

#[cfg(test)]
mod test {
    use serde_json::json;

    use super::*;
    use crate::x509::{RawX509Signer, RustRawX509Signer};

    const CERT_PEM: &'static str = "-----BEGIN CERTIFICATE-----
MIIEKjCCAhKgAwIBAgIBADANBgkqhkiG9w0BAQsFADBFMQswCQYDVQQGEwJVSzEP
MA0GA1UECAwGTG9uZG9uMRMwEQYDVQQKDAplbGVtZW50LmlvMRAwDgYDVQQDDAd0
ZXN0LWNhMB4XDTI2MDQyNzEzMzIzNVoXDTI3MDQyNzEzMzIzNVowKDEmMCQGCSqG
SIb3DQEJARYXdmRoLXg1MDl0ZXN0QG1hdHJpeC5vcmcwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQCzMke0NO4fXtAnkvqqc9PHcf6tMAB7P7+xjWDTpKfJ
PHohC7IzTNR8u1+Oz76fCLq0bIWDbXS570YoA9hlj6UCH5a/+4NhsNYKTapbUVfq
LVk41oNKAFt7brKZ8tkvAG9GCSKztrc3xAG5+QmiEoVY1XSShz3WuD94LIp3me2R
doQcgSP3Gb6zIhpE8sI4MMRsyjU/k822SfiKOqrWKa+HuXTmNDC2UeCTj2++y0e4
s2IRrniOjM84Ci2mMhv/L6cpwwhyI1PsL28tGAX1NhEdVdrQxcOyhh3T9Q2er8J1
kJsfOEmLecNutiSi+mdYbY1pKc60St4KVBE89zAWIg/FAgMBAAGjQjBAMB0GA1Ud
DgQWBBQdCjaG9Xzjyxv9S7xlM1ryXhGyWTAfBgNVHSMEGDAWgBTYXpGaF/DDWxPb
dUJ9ITea3z6WETANBgkqhkiG9w0BAQsFAAOCAgEAhhIcWrFaAd4DuDlHHoqmL9rs
sLn3rx+v4Et+GRV0EjMIOX+tTPO3mUCUb8Ifvk8cC3fucoClKVG8snzIir4eUFOx
d6M7+GAoTLkh8V8XAre3Xo9O2vxB2V8m3IO8Sbnht9aWj9FeMB5Pb9yzTFr8GZPf
7jky+o9R9lrIrURprKBehStYTaNj5YIjYxDXwy+64yXyrckhA8vioFpUFz/CBQMz
B4DwEZRjtFNDxNKA/LcoSnTRfoTpclZIUtJ7YX/qC98QUleuNzpGHM3/59Tn7lzt
ykuWvCAFc3f1nELrEe2ffhCs5M9HNBtGJZ21iEK4LaIu4PBgRTuXDXfsZpC02VKi
s8S9G+er9IUyLpJ1dajY2jXhJuUvmcFLltlzUPYocWcQVVFFAkrrHnew5NzZUPOD
+LQYjNFszITmFc6V49Df23d/kDK+4QzpdtjEf+hjWdDtUDLsAvc48RylwoSIwxn+
3poImhlL5uqHuyQfQqnEVo/DrV44qi3Qv+cBzC+DQ0SQQJ89keWNTGGAVRggY6lX
iQOKT/LtaDO8mtCwY9GT5ZqjynCnLygdG+GgE7sBaRhoj5FFIaXHD83Gcwpv/JI6
giqgGwii4W8KvNkR/k1Z6C9+RvENbluqMRbQ3CXVNDbyU1MB6ZliRGTtbZvL0bd1
/F4LixSyyHVXMUn4/1Q=
-----END CERTIFICATE-----";

    const KEY_PEM: &'static str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCzMke0NO4fXtAn
kvqqc9PHcf6tMAB7P7+xjWDTpKfJPHohC7IzTNR8u1+Oz76fCLq0bIWDbXS570Yo
A9hlj6UCH5a/+4NhsNYKTapbUVfqLVk41oNKAFt7brKZ8tkvAG9GCSKztrc3xAG5
+QmiEoVY1XSShz3WuD94LIp3me2RdoQcgSP3Gb6zIhpE8sI4MMRsyjU/k822SfiK
OqrWKa+HuXTmNDC2UeCTj2++y0e4s2IRrniOjM84Ci2mMhv/L6cpwwhyI1PsL28t
GAX1NhEdVdrQxcOyhh3T9Q2er8J1kJsfOEmLecNutiSi+mdYbY1pKc60St4KVBE8
9zAWIg/FAgMBAAECggEADkeBFpIVMONsRjWxyygPBdib87mx1XW/VRWv/o14tVIg
D/Fk/NjazH8IiVJynSz5O8FeDkVdmidqpBZSWx123P7JaM5Q/4Azk39G259VbcVj
/mHRn8b8LeTPT0czw/QOlI/LzR2lPJ4I3oSYHkBy5yDzThiAIVPDSFSRoCA1oclO
oD/PiXADtfa3vkHTswmnT8JTZP7Dpe1zl5AZALQSFVZyQJ4oFxKqJMWc6LsYpHQ3
AqAAHk7VFyJQGsFZ+D1EgXv3Tzh8YB1nfhjvAucj8VPP9gbDzf4uHEp8FJEHCEAb
lJcEyWNNcpLhSl5T6oY9hjnIkDrxEITdUts+jMRHLQKBgQDrL9ivhSZMZbytU5sv
20JTBFYpgK4TFHeaWnsOwO53AsPTFsl2s6cQ18UNbWIdxn2E7AiTqPVO++E6etNc
HUo76gR9/0gNXILNysQAP0Wzkwtz4c0pXx6pa4zZTihvIcwO0Q6NRAB4sOk+XgDP
Pu79nUSaO8O+d8EUOb+9P2gAZwKBgQDDDfhq12ElgUmhDW92iuD2LwDGg22U4qNQ
0dMXju268TPwx1dYYi/f2yzp/Tn4xKlIo9NZNOzf6GW0+cnOjzyGKCzUglME6Pei
X2Hn8nSD4j60bRCIcf5VfFRAUm9j/y9F/hE5JX+XSM5DKrw09IqzMuu3+Z9TKxRg
dKNOTUQi8wKBgE+d8PLqVl7Cii77AKwgw8Eq1KhUIZnf8eVVABeshI3RZ82MB0Oh
6cqv4Mt83hxKV6+p3/Vs2y6T4llTvz2NxNWnkUG+K/wp9zYHkHas9MGn49ak+Dkr
NEwSVqox5UpJ3LSfXRfBj49MBInSdN+z5GAC33h/BvLxw3E/Y4ODdYe9AoGAaWju
XAbjOBqDiOay2vQ4mLJUD/PMz44fRjjuhCe4r7NUJ4YC3P/K8YYH4rf3kUnuVhQ6
zlW8wVBdTo1DEz7zLWkeuQVpChlAYl57kZbEgtVMn8LlEWfRU69p9IzYJ8kraf7g
nep25nHxDflVVqTlI+yb2IOtJ4v7ahj+e/1jmiMCgYBX57KyNSGew08r2ezjuV6f
wy/qfzJlieWhESZIckJcrDurfzR3xCj4n4xyC4GMTt3bGYs/rGTdkJGxJ6Bq4Ld2
foobh78qDEn1mFVwUuW2kzOJsIlN3FQFSKK3upVdRmzXTvaJN4MKRjwmu6KYIpxA
03GLFlPRHwxXrJAV+om1nQ==
-----END PRIVATE KEY-----";

    #[test]
    fn serialize_x509_signature() {
        // Given an X509Signature
        let sign = RustRawX509Signer::new_from_pem_data(CERT_PEM, KEY_PEM).unwrap();
        let (_id, signature) = sign.sign(&[]).unwrap();

        // When we serialize it
        let json = serde_json::to_value(&signature).unwrap();

        // Then it it is an object
        let json = json.as_object().unwrap();

        // And it has the three properties we expect
        assert_eq!(json.get("signature_scheme").unwrap().as_i64().unwrap(), 2054);
        assert_eq!(json.get("certificate_chain").unwrap().as_str().unwrap(), CERT_PEM);
        assert!(json.get("signature").unwrap().is_string());

        // And no other properties
        let mut keys: Vec<_> = json.keys().collect();
        keys.sort();
        assert_eq!(keys, ["certificate_chain", "signature", "signature_scheme"]);
    }

    #[test]
    fn deserialize_x509_signature() {
        // Given the JSON form of a signature
        let signature = "\
            goZwc8xROWmIMvS2GQb6ONQ/e1qj+d2Q/gJaShR2iQ3sjdlzwwAQdkIAP1zQxP/ak\
            4m1vVBCW7lQN5T2R19fMW4FHGIXYIOxL4joCIV8e8Bge+pIfYDznz+HFtQ5DIApyZ\
            O19wYCNLwaOJtX1TIUWhsz2hTriFunG4Rutcv9uleXxJ3GKCNbFzWV8y/UQxo8JDm\
            jHgmbf4cBUByop0/XYY8vrksdMDrnjJKwp3s3Dm4u5PTo1dGzLosurOpS4X1SkR7g\
            sgvc3x3XzyFCR5JQp/jc3Rn63jE9A4D+kmqsVLYI/zNM6m5euzRm9sAq2lrHR9f8d\
            d4Oj00kGy8dawmvJQ";

        let json = json!({
            "signature_scheme": 2054,
            "certificate_chain": CERT_PEM,
            "signature": signature,
        });

        // When we deserialize it
        let sig: X509Signature = serde_json::from_value(json.clone()).unwrap();

        // Then it contains the properties we supplied
        assert_eq!(sig.signature_scheme, SignatureScheme::RSA_PSS_SHA512);
        assert_eq!(sig.certificate_chain, CERT_PEM);
        assert_eq!(sig.signature, signature);

        // And it round-trips to the same JSON
        let roundtripped = serde_json::to_value(&sig).unwrap();
        assert_eq!(json, roundtripped);
    }
}
