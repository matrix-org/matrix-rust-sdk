#!/bin/bash
set -e
export SYNAPSE_SERVER_NAME=matrix-sdk.rs
export SYNAPSE_REPORT_STATS=no
echo " ====== Generating config  ====== "
/start.py generate

# Courtesy of https://github.com/michaelkaye/setup-matrix-synapse/blob/e2067245b5265640f94d310fc79a1d388cc70452/create.js#L99
echo " ====== Patching for CI  ====== "
echo """
enable_registration: true
enable_registration_without_verification: true

rc_message:
 per_second: 1000
 burst_count: 1000

rc_registration:
 per_second: 1000
 burst_count: 1000

rc_login:
 address:
  per_second: 1000
  burst_count: 1000
 account:
  per_second: 1000
  burst_count: 1000
 failed_attempts:
  per_second: 1000
  burst_count: 1000

rc_admin_redaction:
  per_second: 1000
  burst_count: 1000

rc_joins:
 local:
  per_second: 1000
  burst_count: 1000
 remote:
  per_second: 1000
  burst_count: 1000

rc_3pid_validation:
  per_second: 1000
  burst_count: 1000

rc_invites:
 per_room:
  per_second: 1000
  burst_count: 1000
 per_user:
  per_second: 1000
  burst_count: 1000
 per_issuer:
  per_second: 1000
  burst_count: 1000

experimental_features:
 msc3266_enabled: true
""" >>  /data/homeserver.yaml

echo " ====== Starting server with:  ====== "
cat /data/homeserver.yaml
echo  " ====== STARTING  ====== " 
/start.py run
