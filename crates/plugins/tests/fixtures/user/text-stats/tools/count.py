import json, re, sys
args = json.load(sys.stdin)
text = open(args['file'], encoding='utf-8').read()
print(f"{len(text.split())} words, {len(re.findall(r'[.!?]+', text))} sentences")
