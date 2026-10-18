# YouTube egress proxy: yt-dlp's only way out, past the host's VPN exit node.
# See tinyproxy.conf and docker-compose.yml (yt-proxy) for why.
FROM alpine:3.21
RUN apk add --no-cache tinyproxy
COPY tinyproxy.conf /etc/tinyproxy/tinyproxy.conf
COPY filter /etc/tinyproxy/filter
USER tinyproxy
EXPOSE 8888
CMD ["tinyproxy", "-d", "-c", "/etc/tinyproxy/tinyproxy.conf"]
