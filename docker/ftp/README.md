Generate a self-signed test certificate for FTPS:
`openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj /CN=localhost -keyout docker/ftp/key.pem -out docker/ftp/cert.pem`
