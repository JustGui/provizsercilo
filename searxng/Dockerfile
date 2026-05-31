FROM searxng/searxng:latest
COPY settings.yml /etc/searxng/settings.yml
COPY limiter.toml /etc/searxng/limiter.toml
# Suppress log spam: direct container-to-container access never has proxy headers
RUN sed -i \
    's/log_error_only_once("X-Forwarded-For nor X-Real-IP header is set!")/pass/' \
    /usr/local/searxng/searx/botdetection/trusted_proxies.py \
    && rm -f /usr/local/searxng/searx/botdetection/__pycache__/trusted_proxies.cpython-*.pyc
