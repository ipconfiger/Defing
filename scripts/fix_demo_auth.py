import re

# ---------- dev-single-demo.sh ----------
p = '/home/alex/Projects/Defing/scripts/dev-single-demo.sh'
lines = open(p).read().split('\n')
out = []
done_start = False
done_login = False
for l in lines:
    if not done_start and '--dev-single --http-addr' in l:
        l = l.replace('--http-addr', '--admin-password admin123 --http-addr')
        done_start = True
    out.append(l)
    if not done_login and 'healthz OK' in l:
        out += [
            "TOKEN=$(curl -sf -X POST $BASE/api/v1/login -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")",
            'AUTH="Authorization: Bearer $TOKEN"',
            'echo "  admin login ok"',
        ]
        done_login = True
        continue
login_idx = next(i for i, l in enumerate(out) if 'admin login ok' in l)
for i in range(login_idx + 1, len(out)):
    if 'curl -sf ' in out[i] or 'curl -s ' in out[i]:
        out[i] = out[i].replace('curl -sf ', 'curl -sf -H "$AUTH" ').replace('curl -s ', 'curl -s -H "$AUTH" ')
open(p, 'w').write('\n'.join(out))
print('dev-single-demo auth OK')

# ---------- cluster-demo.sh ----------
p2 = '/home/alex/Projects/Defing/scripts/cluster-demo.sh'
lines2 = open(p2).read().split('\n')
out2 = []
done_start2 = False
done_login2 = False
for l in lines2:
    if not done_start2 and '--data-dir "$data" "$@"' in l:
        l = l.replace('--data-dir "$data" "$@"', '--data-dir "$data" --admin-password admin123 "$@"')
        done_start2 = True
    out2.append(l)
    if not done_login2 and 'node1 ready' in l:
        out2 += [
            'api() { # 按目标主机 token 文件',
            '  local url=""; for a in "$@"; do case "$a" in http://*) url="$a";; esac; done',
            '  local host=$(echo "$url" | cut -d'/' -f3)',
            '  local tok=$(cat "$WORK/tok_"$host 2>/dev/null)',
            '  curl -sf -H "Authorization: Bearer $tok" "$@"',
            '}',
            'for H in "$H1" "$H2" "$H3"; do',
            '  HOST=$(echo "$H" | cut -d'/' -f3)',
            "  TOK=$(curl -sf -X POST "$H/api/v1/login" -H 'Content-Type: application/json' -d '{"password":"admin123"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")",
            '  echo "$TOK" > "$WORK/tok_"$HOST',
            'done',
            'echo "  admin login ok (3 nodes)"',
        ]
        done_login2 = True
        continue
login_idx2 = next(i for i, l in enumerate(out2) if 'admin login ok (3 nodes)' in l)
for i in range(login_idx2 + 1, len(out2)):
    if out2[i].lstrip().startswith('curl '):
        out2[i] = re.sub(r'^(\s*)curl ', r'\1api ', out2[i])
open(p2, 'w').write('\n'.join(out2))
print('cluster-demo auth OK')
