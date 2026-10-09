TLS certificate for the FTP test servers, signed by a throwaway test CA (verification stays
on in the tests). Generate it before starting the services:

```bash
scripts/ftp-test-certs.sh            # writes ca.pem, cert.pem, key.pem here (git-ignored)
docker compose -f docker/compose.yml up -d ftp ftp-implicit ftp-plain
SWITCHYARD_FTP_CA=docker/ftp/ca.pem \
  cargo test -p switchyard-remote --test ftp -- --ignored --test-threads 1
```

Services (user `deploy`, password `switchyard`):

| Service | Port | TLS | Passive ports |
| --- | --- | --- | --- |
| `ftp` | 2121 | explicit (`AUTH TLS`), required | 21000–21010 |
| `ftp-implicit` | 2990 | implicit | 21011–21019 |
| `ftp-plain` | 127.0.0.1:2120 (host network, for active mode) | none | 21020–21030 |
