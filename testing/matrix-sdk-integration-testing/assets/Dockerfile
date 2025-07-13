FROM docker.io/matrixdotorg/synapse:v1.117.0
ADD ci-start.sh /ci-start.sh
RUN chmod 770 /ci-start.sh
ENTRYPOINT /ci-start.sh
