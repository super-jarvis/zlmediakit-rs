#!/usr/bin/env python3
"""Minimal RTMP play client to debug what our server sends."""
import socket
import struct
import time
import sys

def send_bytes(s, data):
    print(f"  SEND {len(data)} bytes: {data[:80].hex()}")
    s.sendall(data)

def recv_bytes(s, n):
    data = b''
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            return data
        data += chunk
    return data

def read_basic_header(data, pos):
    b = data[pos]
    fmt = (b >> 6) & 3
    csid_raw = b & 0x3F
    if csid_raw == 0:
        return fmt, data[pos+1] + 64, pos + 2
    elif csid_raw == 1:
        return fmt, data[pos+1] + data[pos+2] * 256 + 64, pos + 3
    else:
        return fmt, csid_raw, pos + 1

def parse_chunk(data, pos, chunk_size, states):
    if pos >= len(data):
        return None, pos
    fmt, csid, pos = read_basic_header(data, pos)
    
    if fmt == 0:
        if pos + 11 > len(data):
            return None, pos
        ts = struct.unpack('>I', b'\x00' + data[pos:pos+3])[0]
        msg_len = struct.unpack('>I', b'\x00' + data[pos+3:pos+6])[0]
        type_id = data[pos+6]
        msg_sid = struct.unpack('<I', data[pos+7:pos+11])[0]
        pos += 11
        has_ext = False
        if ts == 0xFFFFFF:
            ts = struct.unpack('>I', data[pos:pos+4])[0]
            pos += 4
            has_ext = True
        states[csid] = {'ts': ts, 'len': msg_len, 'type': type_id, 'sid': msg_sid, 'buf': b'', 'has_ext': has_ext}
    elif fmt == 1:
        if pos + 7 > len(data):
            return None, pos
        ts = struct.unpack('>I', b'\x00' + data[pos:pos+3])[0]
        msg_len = struct.unpack('>I', b'\x00' + data[pos+3:pos+6])[0]
        type_id = data[pos+6]
        pos += 7
        has_ext = False
        if ts == 0xFFFFFF:
            ts = struct.unpack('>I', data[pos:pos+4])[0]
            pos += 4
            has_ext = True
        states[csid]['ts'] = ts
        states[csid]['len'] = msg_len
        states[csid]['type'] = type_id
        states[csid]['buf'] = b''
        states[csid]['has_ext'] = has_ext
    elif fmt == 2:
        if pos + 3 > len(data):
            return None, pos
        ts = struct.unpack('>I', b'\x00' + data[pos:pos+3])[0]
        pos += 3
        has_ext = False
        if ts == 0xFFFFFF:
            ts = struct.unpack('>I', data[pos:pos+4])[0]
            pos += 4
            has_ext = True
        states[csid]['ts'] += ts
        states[csid]['buf'] = b''
        states[csid]['has_ext'] = has_ext
    elif fmt == 3:
        if csid in states and states[csid].get('has_ext', False):
            pos += 4  # extended timestamp
    
    state = states.get(csid)
    if state is None:
        return None, pos
    
    remaining = state['len'] - len(state['buf'])
    to_read = min(remaining, chunk_size)
    if pos + to_read > len(data):
        return None, pos
    state['buf'] += data[pos:pos+to_read]
    pos += to_read
    
    if len(state['buf']) >= state['len']:
        msg = {
            'csid': csid,
            'type': state['type'],
            'ts': state['ts'],
            'sid': state['sid'],
            'data': state['buf'][:state['len']]
        }
        state['buf'] = b''
        return msg, pos
    return None, pos

def parse_amf0(data):
    pos = 0
    values = []
    while pos < len(data):
        type_id = data[pos]; pos += 1
        if type_id == 0x00:
            val = struct.unpack('>d', data[pos:pos+8])[0]
            values.append(('number', val))
            pos += 8
        elif type_id == 0x01:
            values.append(('boolean', data[pos] != 0))
            pos += 1
        elif type_id == 0x02:
            slen = struct.unpack('>H', data[pos:pos+2])[0]
            pos += 2
            values.append(('string', data[pos:pos+slen].decode('utf-8', errors='replace')))
            pos += slen
        elif type_id == 0x03:
            obj = {}
            while True:
                if pos + 3 > len(data):
                    break
                kl = struct.unpack('>H', data[pos:pos+2])[0]
                pos += 2
                if kl == 0:
                    if data[pos] == 0x09:
                        pos += 1
                    break
                key = data[pos:pos+kl].decode('utf-8', errors='replace')
                pos += kl
                # Just skip nested values
                if pos < len(data):
                    if data[pos] == 0x00:
                        val = struct.unpack('>d', data[pos+1:pos+9])[0]
                        obj[key] = val
                        pos += 9
                    elif data[pos] == 0x02:
                        sl = struct.unpack('>H', data[pos+1:pos+3])[0]
                        obj[key] = data[pos+3:pos+3+sl].decode('utf-8', errors='replace')
                        pos += 3 + sl
                    elif data[pos] == 0x05:
                        obj[key] = None
                        pos += 1
                    else:
                        obj[key] = f"(type=0x{data[pos]:02x})"
                        pos += 1
            values.append(('object', obj))
        elif type_id == 0x05:
            values.append(('null', None))
        elif type_id == 0x06:
            values.append(('undefined', None))
        elif type_id == 0x08:
            pos += 4  # skip count
            # treat like object
            obj = {}
            while True:
                if pos + 3 > len(data):
                    break
                kl = struct.unpack('>H', data[pos:pos+2])[0]
                pos += 2
                if kl == 0:
                    if pos < len(data) and data[pos] == 0x09:
                        pos += 1
                    break
                key = data[pos:pos+kl].decode('utf-8', errors='replace')
                pos += kl
                if pos < len(data):
                    if data[pos] == 0x00:
                        val = struct.unpack('>d', data[pos+1:pos+9])[0]
                        obj[key] = val
                        pos += 9
                    elif data[pos] == 0x02:
                        sl = struct.unpack('>H', data[pos+1:pos+3])[0]
                        obj[key] = data[pos+3:pos+3+sl].decode('utf-8', errors='replace')
                        pos += 3 + sl
                    elif data[pos] == 0x05:
                        obj[key] = None
                        pos += 1
                    else:
                        obj[key] = f"(type=0x{data[pos]:02x})"
                        pos += 1
            values.append(('ecma_array', obj))
        else:
            values.append((f'unknown_0x{type_id:02x}', data[pos:pos+20].hex()))
            break
    return values

host = '127.0.0.1'
port = 1935
stream = sys.argv[1] if len(sys.argv) > 1 else 'test'

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect((host, port))
s.settimeout(5)

print("=== HANDSHAKE ===")
# C0+C1
c0c1 = bytes([3]) + b'\x00' * 1536
send_bytes(s, c0c1)

# Read S0+S1+S2
s0s1s2 = recv_bytes(s, 1 + 1536 + 1536)
print(f"  RECV S0S1S2: {len(s0s1s2)} bytes, S0={s0s1s2[0]:02x}")

# Send C2 (echo S1)
send_bytes(s, s0s1s2[1:1+1536])

print("\n=== CONNECT ===")

# Window Ack Size + Set Peer Bandwidth + Set Chunk Size from server will come
# after our connect command. But first let's build the connect command.

def make_chunk(csid, ts, msg_len, type_id, sid, payload, chunk_size=128):
    """Build RTMP chunk(s) for a message."""
    result = b''
    # Basic header
    if csid < 64:
        result += bytes([csid])
    elif csid < 64 + 256:
        result += bytes([0, csid - 64])
    else:
        result += bytes([1, (csid - 64) & 0xFF, (csid - 64) >> 8])
    
    ts_field = min(ts, 0xFFFFFF)
    result += struct.pack('>I', ts_field)[1:]  # 3 bytes
    result += struct.pack('>I', msg_len)[1:]  # 3 bytes
    result += bytes([type_id])
    result += struct.pack('<I', sid)  # 4 bytes LE
    
    if ts >= 0xFFFFFF:
        result += struct.pack('>I', ts)
    
    # Payload in chunks
    offset = 0
    while offset < len(payload):
        end = min(offset + chunk_size, len(payload))
        result += payload[offset:end]
        offset = end
        if offset < len(payload):
            if csid < 64:
                result += bytes([csid])
            elif csid < 64 + 256:
                result += bytes([0, csid - 64])
            else:
                result += bytes([1, (csid - 64) & 0xFF, (csid - 64) >> 8])
    
    return result

# Build connect command
import io
amf = io.BytesIO()
amf.write(b'\x02')  # string
amf.write(struct.pack('>H', 7))
amf.write(b'connect')
amf.write(b'\x00')  # number
amf.write(struct.pack('>d', 1.0))
amf.write(b'\x03')  # object
# tcUrl
amf.write(struct.pack('>H', 5))
amf.write(b'tcUrl')
amf.write(b'\x02')
amf.write(struct.pack('>H', len(f'rtmp://{host}:{port}/live')))
amf.write(f'rtmp://{host}:{port}/live'.encode())
# objectEncoding
amf.write(struct.pack('>H', 14))
amf.write(b'objectEncoding')
amf.write(b'\x00')
amf.write(struct.pack('>d', 0.0))
# end object
amf.write(b'\x00\x00\x09')

connect_payload = amf.getvalue()
chunk = make_chunk(3, 0, len(connect_payload), 20, 0, connect_payload)
send_bytes(s, chunk)

# Read server responses
print("\n=== READING SERVER RESPONSES ===")
states = {}
chunk_size = 128
msg_count = 0

while msg_count < 20:
    try:
        raw = recv_bytes(s, 8192)
    except socket.timeout:
        print("  TIMEOUT waiting for data")
        break
    
    if not raw:
        print("  Connection closed")
        break
    
    print(f"\n  RAW {len(raw)} bytes: {raw[:120].hex()}")
    
    pos = 0
    while pos < len(raw):
        msg, new_pos = parse_chunk(raw, pos, chunk_size, states)
        if msg is None:
            break
        pos = new_pos
        msg_count += 1
        
        type_names = {1:'SetChunkSize', 2:'Abort', 3:'Ack', 4:'UserControl', 5:'WindowAckSize', 6:'SetPeerBw', 8:'Audio', 9:'Video', 18:'AmfData', 20:'AmfCmd'}
        tname = type_names.get(msg['type'], f'type_{msg["type"]}')
        print(f"  MSG #{msg_count}: csid={msg['csid']} type={tname}({msg['type']}) ts={msg['ts']} sid={msg['sid']} len={len(msg['data'])}")
        
        if msg['type'] == 1:  # SetChunkSize
            new_cs = struct.unpack('>I', msg['data'][:4])[0]
            print(f"    SetChunkSize: {new_cs}")
            chunk_size = new_cs
        elif msg['type'] == 5:  # WindowAckSize
            wsize = struct.unpack('>I', msg['data'][:4])[0]
            print(f"    WindowAckSize: {wsize}")
        elif msg['type'] == 6:  # SetPeerBandwidth
            wsize = struct.unpack('>I', msg['data'][:4])[0]
            print(f"    SetPeerBandwidth: {wsize}")
        elif msg['type'] == 4:  # UserControl
            evt = struct.unpack('>H', msg['data'][:2])[0]
            print(f"    UserControl event: {evt}")
        elif msg['type'] in (20, 18):  # AMF0
            vals = parse_amf0(msg['data'])
            print(f"    AMF0: {vals}")
        else:
            print(f"    data: {msg['data'][:60].hex()}")
        
        # After getting initial responses, send createStream
        if msg_count == 5:  # After connect responses
            print("\n=== SENDING createStream ===")
            amf2 = io.BytesIO()
            amf2.write(b'\x02')  # string
            amf2.write(struct.pack('>H', 12))
            amf2.write(b'createStream')
            amf2.write(b'\x00')  # number
            amf2.write(struct.pack('>d', 2.0))
            amf2.write(b'\x05')  # null
            cs_payload = amf2.getvalue()
            chunk = make_chunk(3, 0, len(cs_payload), 20, 0, cs_payload)
            send_bytes(s, chunk)
    
    if msg_count >= 5:
        break

# Now wait for createStream response, then send play
print("\n=== WAITING FOR createStream RESPONSE ===")
while True:
    try:
        raw = recv_bytes(s, 8192)
    except socket.timeout:
        print("  TIMEOUT")
        break
    if not raw:
        break
    
    pos = 0
    while pos < len(raw):
        msg, new_pos = parse_chunk(raw, pos, chunk_size, states)
        if msg is None:
            break
        pos = new_pos
        msg_count += 1
        type_names = {1:'SetChunkSize', 2:'Abort', 3:'Ack', 4:'UserControl', 5:'WindowAckSize', 6:'SetPeerBw', 8:'Audio', 9:'Video', 18:'AmfData', 20:'AmfCmd'}
        tname = type_names.get(msg['type'], f'type_{msg["type"]}')
        print(f"  MSG #{msg_count}: csid={msg['csid']} type={tname}({msg['type']}) ts={msg['ts']} sid={msg['sid']} len={len(msg['data'])}")
        
        if msg['type'] == 20:
            vals = parse_amf0(msg['data'])
            print(f"    AMF0: {vals}")
            if vals and vals[0][1] == '_result':
                print("\n=== SENDING play ===")
                amf3 = io.BytesIO()
                amf3.write(b'\x02')  # string
                amf3.write(struct.pack('>H', 4))
                amf3.write(b'play')
                amf3.write(b'\x00')  # number
                amf3.write(struct.pack('>d', 3.0))
                amf3.write(b'\x05')  # null
                amf3.write(b'\x02')  # string
                amf3.write(struct.pack('>H', len(stream)))
                amf3.write(stream.encode())
                play_payload = amf3.getvalue()
                chunk = make_chunk(3, 0, len(play_payload), 20, 1, play_payload)
                send_bytes(s, chunk)
                print("  Play sent! Now reading responses...")
                
                # Read play responses
                for i in range(50):
                    try:
                        raw2 = recv_bytes(s, 16384)
                    except socket.timeout:
                        print("  TIMEOUT reading play response")
                        break
                    if not raw2:
                        print("  Connection closed")
                        break
                    
                    print(f"\n  PLAY RECV {len(raw2)} bytes")
                    p2 = 0
                    while p2 < len(raw2):
                        msg2, new_p2 = parse_chunk(raw2, p2, chunk_size, states)
                        if msg2 is None:
                            break
                        p2 = new_p2
                        msg_count += 1
                        tname2 = type_names.get(msg2['type'], f'type_{msg2["type"]}')
                        data_preview = msg2['data'][:60].hex() if len(msg2['data']) > 60 else msg2['data'].hex()
                        print(f"    PLAY MSG #{msg_count}: csid={msg2['csid']} type={tname2}({msg2['type']}) ts={msg2['ts']} sid={msg2['sid']} len={len(msg2['data'])}")
                        if msg2['type'] == 20:
                            vals2 = parse_amf0(msg2['data'])
                            print(f"      AMF0: {vals2}")
                        elif msg2['type'] == 4:
                            evt = struct.unpack('>H', msg2['data'][:2])[0]
                            print(f"      UserControl event: {evt}")
                        elif msg2['type'] in (8, 9):
                            print(f"      Media data ({len(msg2['data'])} bytes): {msg2['data'][:20].hex()}")
                
                sys.exit(0)
            elif vals and vals[0][1] == '_error':
                print(f"    ERROR: {vals}")
                sys.exit(1)

s.close()
