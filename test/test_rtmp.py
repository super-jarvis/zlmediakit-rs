#!/usr/bin/env python3
"""Simple RTMP handshake + connect test"""
import socket
import struct
import time
import os

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('127.0.0.1', 19350))
sock.settimeout(5)

# C0
sock.sendall(b'\x03')

# C1 (1536 bytes: timestamp=0, version=0, random)
c1 = struct.pack('>II', 0, 0) + os.urandom(1528)
sock.sendall(c1)

# S0
s0 = sock.recv(1)
print(f"S0: {s0.hex()}")
assert s0 == b'\x03', f"Expected S0=0x03, got {s0.hex()}"

# S1 (1536 bytes)
s1 = sock.recv(1536)
print(f"S1 len: {len(s1)}")
assert len(s1) == 1536, f"Expected 1536 bytes S1, got {len(s1)}"

# S2 (1536 bytes, echo of C1)
s2 = sock.recv(1536)
print(f"S2 len: {len(s2)}")
assert len(s2) == 1536, f"Expected 1536 bytes S2, got {len(s2)}"

# C2 (echo of S1)
sock.sendall(s1)

print("Handshake completed!")
time.sleep(0.5)

# Now try to send a simple AMF connect command
# RTMP message: type 20 (AMF0 command), stream 0
import struct

def make_amf_connect():
    """Build AMF0 connect command payload"""
    payload = b''
    # Command name: "connect"
    payload += b'\x02'  # AMF0 String
    payload += struct.pack('>H', 7)
    payload += b'connect'
    # Transaction ID: 1
    payload += b'\x00'  # AMF0 Number
    payload += struct.pack('>d', 1.0)
    # Command object
    payload += b'\x03'  # AMF0 Object
    # tcUrl
    payload += struct.pack('>H', 5)
    payload += b'tcUrl'
    payload += b'\x02'  # AMF0 String
    tc_url = b'rtmp://localhost:19350/live'
    payload += struct.pack('>H', len(tc_url))
    payload += tc_url
    # fpad
    payload += struct.pack('>H', 4)
    payload += b'fpad'
    payload += b'\x01'  # Boolean
    payload += b'\x00'
    # capabilities
    payload += struct.pack('>H', 12)
    payload += b'capabilities'
    payload += b'\x00'
    payload += struct.pack('>d', 239.0)
    # End object
    payload += b'\x00\x00\x09'
    return payload

amf_payload = make_amf_connect()

# Build RTMP chunk: type 20 (AMF0 command), csid 3, timestamp 0, stream 0
csid = 3
type_id = 20
stream_id = 0
timestamp = 0

msg_len = len(amf_payload)

# Basic header: fmt=0, csid=3
basic_header = bytes([(0 << 6) | csid])
# Message header (11 bytes): timestamp(3) + msg_len(3) + type_id(1) + stream_id(4)
msg_header = struct.pack('>I', timestamp)[1:]  # 3 bytes (big endian, skip first)
msg_header += struct.pack('>I', msg_len)[1:]    # 3 bytes
msg_header += struct.pack('B', type_id)          # 1 byte
msg_header += struct.pack('<I', stream_id)       # 4 bytes (little endian)

chunk = basic_header + msg_header + amf_payload
print(f"Sending connect command ({len(chunk)} bytes)...")
sock.sendall(chunk)

# Wait for response
time.sleep(1)
try:
    data = sock.recv(65536)
    print(f"Received {len(data)} bytes response")
    print(f"Response hex (first 100 bytes): {data[:100].hex()}")
    # Parse the basic header
    if data:
        basic = data[0]
        fmt = (basic >> 6) & 3
        csid_raw = basic & 63
        print(f"  fmt={fmt}, csid={csid_raw}")
except socket.timeout:
    print("No response received (timeout)")

sock.close()
