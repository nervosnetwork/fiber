# Fiber Node Network Connection TLS Configuration Guide
> Browsers cannot connect to P2P networks directly. Instead, they must connect to your node over WebSocket with TLS (wss://). To support this, you need to deploy a TLS proxy in front of your node that converts WSS traffic into standard TCP.

Before you begin, ensure the following:

- Your Fiber node is running and listening on its default P2P port (8228)
- You own a domain name and can modify its DNS records (e.g., via Cloudflare, Namecheap, or Alibaba Cloud).
- You have a valid TLS certificate for your domain (You can use a commercial provider like DigiCert or a free tool like Certbot.)
- Your server has ports 80 and 443 open to the public.
- Nginx is installed with the Stream module enabled, which will be used to forward encrypted WebSocket traffic to the Fiber node.

## Install the Nginx Stream Module

```
sudo apt update
sudo apt install -y build-essential libpcre3 libpcre3-dev zlib1g zlib1g-dev libssl-dev jq
```

```
cd /opt/
sudo wget https://nginx.org/download/nginx-1.24.0.tar.gz
sudo tar -zxvf nginx-1.24.0.tar.gz
cd nginx-1.24.0

sudo ./configure \
  --prefix=/usr/local/nginx \
  --with-stream \
  --with-stream_ssl_module \
  --with-stream_ssl_preread_module \
  --with-http_ssl_module \
  --with-http_v2_module \
  --with-http_gzip_static_module \
  --with-http_stub_status_module \
  --with-http_realip_module \
  --with-pcre
```

```
sudo make
sudo make install
```

## Working Example of Nginx Config

Here’s a working example that:

- Routes TCP (P2P) and WSS traffic differently based on TLS version
- Proxies WSS traffic to a separate internal port (8443)

You need to replace:

- fiber.example.com with your domain
- 8228 with your node’s P2P port
- 443 with the external public port
- TLS cert paths with your actual file paths

```
# Global settings
user www-data;
worker_processes auto;
pid /run/nginx.pid;

# Events
events {
    worker_connections 768;
}

# ===================
# Stream block (used for TCP/TLS protocol detection and routing)
# ===================
stream {
    log_format stream_log '$remote_addr - $remote_port [$time_local] '
                          'Protocol: $ssl_preread_protocol '
                          'Status: $status '
                          'Bytes_Sent: $bytes_sent '
                          'Bytes_Received: $bytes_received '
                          'Session_Time: $session_time '
                          'Upstream: $upstream_addr';

    # Map TLS version to upstream
    map $ssl_preread_protocol $upstream {
        default      backend_tcp;
        "TLSv1.2"    backend_wss_http;
        "TLSv1.3"    backend_wss_http;
    }

    upstream backend_tcp {
        server 127.0.0.1:8228;
    }

    upstream backend_wss_http {
        server 127.0.0.1:8443;
    }

    server {
        listen 443;
        proxy_pass $upstream;
        ssl_preread on;
        access_log /var/log/nginx/stream_access.log stream_log;
        error_log /var/log/nginx/stream_error.log;

    }
}

# ===================
# HTTP block (for WSS over HTTPS and Web services)
# ===================
http {
    sendfile on;
    tcp_nopush on;
    tcp_nodelay on;
    keepalive_timeout 65;
    server_tokens off;
    types_hash_max_size 2048;

    client_max_body_size 10m;

    include /usr/local/nginx/conf/mime.types;
    default_type application/octet-stream;

    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers on;

    access_log /var/log/nginx/access.log;
    error_log /var/log/nginx/error.log;
    gzip on;

    # Server block for WSS proxy
    server {
        listen 8443 ssl;
        server_name fiber.example.com;

        ssl_certificate /usr/local/nginx/ssl/fullchain.pem;
        ssl_certificate_key /usr/local/nginx/ssl/privkey.pem;
        ssl_protocols TLSv1.2 TLSv1.3;

        location / {
            proxy_pass http://127.0.0.1:8228;
            proxy_http_version 1.1;
            proxy_set_header Upgrade $http_upgrade;
            proxy_set_header Connection "upgrade";
            proxy_set_header Host $host;
            proxy_read_timeout 3600s;
            proxy_send_timeout 3600s;
        }
    }
}
```

## Start nginx
```
/usr/local/nginx/sbin/nginx -t
/usr/local/nginx/sbin/nginx
# /usr/local/nginx/sbin/nginx -s reload
```

## Add Domain DNS Resolution

Add an A record for the domain fiber.example.com to your Fiber node's IP address in your domain registrar.

## Get Your node's addresses

Prerequisites:

- Fiber node config.yml announced_addrs add public IP
- Your node's RPC port is 8227

```
curl -s -X POST http://127.0.0.1:8227 -H "Content-Type: application/json" -d '{
    "id": 42,
    "jsonrpc": "2.0",
    "method": "node_info",
    "params": []
}' |jq '.result.addresses[0]' |awk -F '/p2p/' '{print $2}'
```

## Connect via the WSS address
/dns4/{Your-Domain-Name}/tcp/443/wss/p2p/{Your-Fiber-Node-Addresses}
