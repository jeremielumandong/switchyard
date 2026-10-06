#!/usr/bin/env python3
"""Regenerate the Driver Manager signature fixtures.

TEST-ONLY key derived from a fixed seed; never use it for real manifests. Real manifests
are signed with the maintainers' key: `minisign -S -m manifest.json`.
Output format follows minisign (prehashed `ED`: Ed25519 over BLAKE2b-512 of the file).
"""
import base64, hashlib, os, sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization

here = os.path.dirname(os.path.abspath(__file__))
seed = hashlib.sha256(b"switchyard driver manager test key").digest()
key = Ed25519PrivateKey.from_private_bytes(seed)
pk = key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
key_id = hashlib.sha256(pk).digest()[:8]

def sign(data: bytes, trusted: str) -> str:
    sig = key.sign(hashlib.blake2b(data, digest_size=64).digest())
    glob = key.sign(sig + trusted.encode())
    return (
        "untrusted comment: signature from switchyard TEST key\n"
        + base64.b64encode(b"ED" + key_id + sig).decode() + "\n"
        + "trusted comment: " + trusted + "\n"
        + base64.b64encode(glob).decode() + "\n"
    )

manifest = open(os.path.join(here, "manifest.json"), "rb").read()
open(os.path.join(here, "manifest.json.minisig"), "w").write(sign(manifest, "timestamp:0\tfile:manifest.json"))
open(os.path.join(here, "test.pub"), "w").write(base64.b64encode(b"Ed" + key_id + pk).decode() + "\n")
