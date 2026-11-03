import json, struct, sys, zlib
path = sys.argv[1]
data = open(path, "rb").read()
off = 0
seq = 1
rows = []
while len(data) - off >= 20:
    magic, frame_seq, length, crc = struct.unpack_from("<IQII", data, off)
    if magic != 0x564A4C31 or frame_seq != seq:
        break
    payload = data[off + 20:off + 20 + length]
    if len(payload) != length or (zlib.crc32(payload) & 0xFFFFFFFF) != crc:
        break
    end = off + 20 + length
    rows.append({"start": off, "end": end, "sequence": frame_seq, "payload_len": length})
    off = end
    seq += 1
print(json.dumps({"frames": rows, "valid_prefix": off, "file_size": len(data)}))
