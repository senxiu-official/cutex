"""Build a pinned LF export with JSON parse counters, without editing product sources."""
import argparse, hashlib, json, os, pathlib, re, shutil, subprocess, tarfile

def main():
    p=argparse.ArgumentParser(); p.add_argument('--scratch',type=pathlib.Path,required=True); p.add_argument('--revision',default='HEAD'); a=p.parse_args()
    repo=pathlib.Path(__file__).resolve().parents[2]
    a.scratch.mkdir(parents=True,exist_ok=True)
    dst=a.scratch/'instrumented'
    if dst.exists(): raise SystemExit(f'refusing to overwrite {dst}')
    archive=a.scratch/'source.tar'
    subprocess.run(['git','-C',str(repo),'-c','core.autocrlf=false','archive','--format=tar','-o',str(archive),a.revision],check=True)
    with tarfile.open(archive) as t: t.extractall(dst,filter='data')
    changed={}
    # Count every direct JSON text/bytes/reader decode in both library and binary.
    # from_value is a typed conversion, not a text parse, and is deliberately excluded.
    for f in (dst/'src').rglob('*.rs'):
        prefix='cutex' if f.is_relative_to(dst/'src'/'cli_app') or f.name=='main.rs' else 'crate'
        s=f.read_text(encoding='utf-8'); s,n=re.subn(r'\bserde_json::(from_str|from_slice|from_reader)\b',rf'{prefix}::scale_metrics::\1',s)
        if n: f.write_text(s,encoding='utf-8',newline='\n'); changed[str(f.relative_to(dst))]=n
    shutil.copyfile(pathlib.Path(__file__).with_name('instrument.rs'),dst/'src'/'scale_metrics.rs')
    f=dst/'src'/'lib.rs'; f.write_text(f.read_text(encoding='utf-8')+'\npub mod scale_metrics;\n',encoding='utf-8',newline='\n')
    f=dst/'src'/'main.rs'; s=f.read_text(encoding='utf-8'); s=s.replace('fn main() {','fn main() {\n    cutex::scale_metrics::start();',1); f.write_text(s,encoding='utf-8',newline='\n')
    f=dst/'src'/'management'/'server.rs'; s=f.read_text(encoding='utf-8'); start=s.index('pub fn handle_parsed_management_request('); brace=s.index(') -> anyhow::Result<()> {',start)+len(') -> anyhow::Result<()> {'); s=s[:brace]+'\n    let _scale_request = crate::scale_metrics::Request::new(&request.path);'+s[brace:]; f.write_text(s,encoding='utf-8',newline='\n')
    manifest=dict(product_sha=subprocess.check_output(['git','-C',str(repo),'rev-parse',a.revision],text=True).strip(),instrumentation_sha256=hashlib.sha256(pathlib.Path(__file__).with_name('instrument.rs').read_bytes()).hexdigest(),parse_call_sites=changed,excluded=['serde_json::from_value (typed conversion)','direct serde_json::Deserializer (workflow logs, avatar compatibility, integration event ledger)'])
    (a.scratch/'instrumentation.json').write_text(json.dumps(manifest,indent=2))
    subprocess.run(['cargo','build','--locked','--bin','cutex'],cwd=dst,check=True)
    target=pathlib.Path(os.environ.get('CARGO_TARGET_DIR',str(dst/'target')))
    (target/'debug'/'scale-gate-build.json').write_text(json.dumps(manifest,indent=2))
if __name__=='__main__': main()
