#!/bin/bash
# Real-git compatibility matrix against a RUNNING node. Not part of `cargo test`: it needs a git
# client, a server, and (for the ssh section) two keys. Exercises client behaviours the Rust
# tests do not: postBuffer chunking, gzip bodies, wide pushes, concurrent clones, --mirror.
#
#   TOKEN     required  — a token for OWNER (cargo run -- admin add-token <owner>)
#   OWNER     default karthik; REPO default demo; FORK default demo-fork — both repos must exist
#   PORT      default 8080 (the PUBLIC listener)
#   SSH_KEY   optional — a private key registered for OWNER; SSH_BAD_KEY an unregistered one.
#             Both set ⇒ the ssh section runs; otherwise it is skipped.
TOKEN="${TOKEN:?run: cargo run --bin kloudlite -- admin add-token <owner>, and export TOKEN}"
OWNER="${OWNER:-karthik}"; REPO="${REPO:-demo}"; FORK="${FORK:-demo-fork}"; PORT="${PORT:-8080}"
T="$TOKEN"
U="http://x:$T@127.0.0.1:$PORT/$OWNER/$REPO.git"
F="http://x:$T@127.0.0.1:$PORT/$OWNER/$FORK.git"
# Scratch outside the repo: the old version littered tests/ with clones.
SP=$(mktemp -d); trap 'rm -rf "$SP"' EXIT; cd "$SP"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_TERMINAL_PROMPT=0
pass=0; fail=0
r(){ local n="$1"; shift; printf "%-46s" "$n"
  if out=$(perl -e 'alarm 240; exec @ARGV' "$@" 2>&1); then echo "OK"; pass=$((pass+1));
  else echo "FAIL: $(echo "$out"|tail -1|cut -c1-90)"; fail=$((fail+1)); fi; }
# drop leftovers from any previous run so results reflect this run only
git ls-remote "$U" 2>/dev/null | awk '/refs\/(tags\/mt|heads\/(mb|mp|mx|mchunk|mgz))/{print $2}' | while read -r ref; do
  git push -q "$U" ":$ref" 2>/dev/null
done

echo "=== basic"
r "ls-remote"                       git ls-remote -q "$U"
r "clone (single-branch)"           git clone -q --single-branch -b main "$U" w
r "clone (all branches)"            git clone -q "$U" x
r "fetch no-op"                     git -C w fetch -q origin
r "push one commit"                 bash -c "cd w && echo a >> a.txt && git commit -qam m1 && git push -q origin HEAD:main"
r "push 10 commits"                 bash -c "cd w && for i in \$(seq 10); do echo \$i >> a.txt; git commit -qam c\$i; done && git push -q origin HEAD:main"
r "branch at existing commit"       bash -c "cd w && git push -q origin HEAD~3:refs/heads/mx1"
r "delete branch"                   bash -c "cd w && git push -q origin --delete mx1"
r "force push"                      bash -c "cd w && git reset -q --hard HEAD~2 && git push -q -f origin HEAD:main"
r "non-ff rejected"                 bash -c "cd w && git commit -q --allow-empty -m nf && ! git push -q origin HEAD:refs/heads/big 2>&1"
r "force-with-lease"                bash -c "cd w && git push -q --force-with-lease origin HEAD:main"

echo "=== tags"
r "annotated tag"                   bash -c "cd w && git tag -a mt1 -m t1 && git push -q origin mt1"
r "lightweight tag"                 bash -c "cd w && git tag mt2 && git push -q origin mt2"
r "push --tags"                     bash -c "cd w && git tag mt3 && git push -q --tags origin"
r "tag on old commit"               bash -c "cd w && git tag mt4 HEAD~2 && git push -q origin mt4"
r "fetch tag only"                  bash -c "rm -rf tg && git init -q tg && cd tg && git fetch -q $U refs/tags/mt1 && git cat-file -t FETCH_HEAD"
r "delete tag"                      bash -c "cd w && git push -q origin --delete mt2"

echo "=== scale / shape"
r "50 branches"                     bash -c "cd w && for i in \$(seq 50); do git branch mb\$i; done && git push -q origin --all"
r "delete 50 branches"              bash -c "cd w && git push -q origin \$(for i in \$(seq 50); do echo :mb\$i; done)"
r "5MB binary"                      bash -c "cd w && head -c 5000000 /dev/urandom > big.bin && git add . && git commit -qm bin && git push -q origin HEAD:main"
r "clone big branch (1000 commits)" git clone -q --single-branch -b big "$U" bigc
r "gc + repush"                     bash -c "cd w && git gc -q 2>/dev/null; git push -q origin HEAD:main"
r "10 concurrent clones"            bash -c "for i in \$(seq 10); do git clone -q --single-branch -b big $U cc\$i & done; wait; for i in \$(seq 10); do [ \$(git -C cc\$i rev-list --count HEAD) = 1000 ] || exit 1; done"
r "concurrent pushes (3 branches)"  bash -c "cd w && for i in 1 2 3; do git push -q origin HEAD:refs/heads/mp\$i & done; wait"

echo "=== transports / protocol"
r "protocol v0 (expect fail)"       bash -c "! git -c protocol.version=0 clone -q $U v0"
r "shallow --depth 1 (expect fail)" bash -c "! git clone -q --depth 1 $U sh"
r "partial --filter=blob:none"      git clone -q --filter=blob:none "$U" pc
r "clone --bare"                    git clone -q --bare "$U" bare
r "push --mirror"                   bash -c "cd bare && git push -q --mirror $U"
r "small http.postBuffer"           bash -c "cd w && git -c http.postBuffer=1024 push -q origin HEAD:refs/heads/mchunk"
r "gzip request body"               bash -c "cd w && git -c http.postBuffer=1048576 push -q origin HEAD:refs/heads/mgz"

echo "=== fork network"
r "clone fork"                      git clone -q --single-branch -b main "$F" f
r "push to fork"                    bash -c "cd f && git commit -q --allow-empty -m forkc && git push -q origin HEAD:main"
FH=$(git -C "$SP/f" rev-parse HEAD 2>/dev/null)
r "fork commit absent from parent"  bash -c "! git -C $SP/w cat-file -e $FH 2>/dev/null"
r "fork commit not advertised"      bash -c "! git ls-remote $U | grep -q $FH"
r "cross-fork SHA fetch refused"    bash -c "! git -C $SP/w fetch -q origin $FH 2>&1"

echo "=== auth"
r "no auth -> 401"                  bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' 'http://127.0.0.1:$PORT/$OWNER/$REPO.git/info/refs?service=git-upload-pack') = 401 ]"
r "bad token -> 401"                bash -c "[ \$(curl -s -o /dev/null -u x:bad -w '%{http_code}' 'http://127.0.0.1:$PORT/$OWNER/$REPO.git/info/refs?service=git-upload-pack') = 401 ]"
r "wrong owner -> 403"              bash -c "[ \$(curl -s -o /dev/null -u x:$T -w '%{http_code}' 'http://127.0.0.1:$PORT/bob/$REPO.git/info/refs?service=git-upload-pack') = 403 ]"
r "unknown repo -> 404"             bash -c "[ \$(curl -s -o /dev/null -u x:$T -w '%{http_code}' 'http://127.0.0.1:$PORT/$OWNER/nope.git/info/refs?service=git-upload-pack') = 404 ]"
r "traversal -> 4xx"                bash -c "c=\$(curl -s -o /dev/null -u x:$T -w '%{http_code}' 'http://127.0.0.1:$PORT/..%2f..%2fetc/passwd.git/info/refs?service=git-upload-pack'); [ \${c:0:1} = 4 ]"

echo "=== ssh"
if [ -z "${SSH_KEY:-}" ] || [ -z "${SSH_BAD_KEY:-}" ]; then
  echo "skipped: set SSH_KEY and SSH_BAD_KEY to run"
else
export GIT_SSH_COMMAND="ssh -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o ConnectTimeout=10 -o LogLevel=ERROR"
S="ssh://git@127.0.0.1:2222/$OWNER/$REPO.git"
r "ssh clone"                       git clone -q --single-branch -b main "$S" s1
r "ssh push"                        bash -c "cd s1 && git commit -q --allow-empty -m sshc && git push -q origin HEAD:main"
r "ssh fetch"                       bash -c "cd s1 && git fetch -q origin"
r "ssh unknown key rejected"        bash -c "GIT_SSH_COMMAND='ssh -i $SSH_BAD_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=10' ; ! GIT_SSH_COMMAND=\"\$GIT_SSH_COMMAND\" git ls-remote $S"
r "ssh interactive session refused" bash -c "! ssh -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o ConnectTimeout=10 -p 2222 git@127.0.0.1 2>/dev/null"
r "http and ssh agree on refs"      bash -c "diff <(git ls-remote $S | sort) <(git ls-remote $U | sort)"
fi

echo "=== integrity"
r "final clone fsck clean"          bash -c "rm -rf fin && git clone -q $U fin && cd fin && git fsck --full 2>&1 | grep -v dangling | grep -q . && exit 1 || exit 0"
echo
echo "RESULT: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
