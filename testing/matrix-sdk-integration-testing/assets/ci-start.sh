#!/bin/bash
set -e
export SYNAPSE_SERVER_NAME=matrix-sdk.rs
export SYNAPSE_REPORT_STATS=no
echo " ====== Generating config  ====== "
/start.py generate
echo " ====== Patching for CI  ====== "
sed -i 's/^#enable_registration_without_verification:.*$/enable_registration_without_verification: true/g' /data/homeserver.yaml
sed -i 's/^#enable_registration:.*$/enable_registration: true/g' /data/homeserver.yaml
echo """

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
#  account:
#    per_second: 0.17
#    burst_count: 3
#  failed_attempts:
#    per_second: 0.17
#    burst_count: 3

""" >>  /data/homeserver.yaml

echo " ====== Starting server with:  ====== "
cat /data/homeserver.yaml
echo  " ====== STARTING  ====== " 
/start.py run