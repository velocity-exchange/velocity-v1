#!/usr/bin/env python3
# Read an `anchor idl build` log on stdin, extract the emitted IDL, strip the
# module path from every type name (reference and definition), stamp the program
# address, and print the result. See gen-quoter-idls.sh for why.
import sys, re, json
addr = sys.argv[1]
s = sys.stdin.read()
m = re.search(r'--- IDL begin program ---\n(.*?)\n--- IDL end program ---', s, re.S)
if not m:
    sys.stderr.write("no IDL block in build output\n"); sys.exit(1)
d = json.loads(m.group(1))
def strip(o):
    if isinstance(o, dict):
        dv = o.get('defined')
        if isinstance(dv, dict) and 'name' in dv:
            dv['name'] = dv['name'].split('::')[-1]
        for v in o.values(): strip(v)
    elif isinstance(o, list):
        for v in o: strip(v)
strip(d)
for t in d.get('types', []):    t['name'] = t['name'].split('::')[-1]
for a in d.get('accounts', []): a['name'] = a['name'].split('::')[-1]
d['address'] = addr
print(json.dumps(d, indent=2))
