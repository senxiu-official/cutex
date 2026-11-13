"""Native publication uses real Minisign checks and an offline GitHub asset store."""
import json,os,shutil,subprocess,tempfile,unittest
from pathlib import Path

SCRIPT=Path(__file__).with_name('sign-and-upload.sh')
@unittest.skipUnless(shutil.which('bash') and shutil.which('minisign'),'Linux signing tools required')
class ImmutableNativeTest(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);self.work=self.root/'work';self.remote=self.root/'remote';self.bin=self.root/'bin'
  for p in [self.work/'native',self.remote,self.bin]:p.mkdir(parents=True)
  self.pub=self.root/'key.pub';self.key=self.root/'key.sec';self.calls=self.root/'calls.jsonl'
  subprocess.run(['minisign','-G','-W','-s',str(self.key),'-p',str(self.pub)],check=True,capture_output=True)
  self.name='native-0.167.0-r60-linux-x64.tar.gz';self.archive=self.work/'native'/self.name;self.archive.write_bytes(b'canonical native archive\n')
  subprocess.run(['minisign','-S','-s',str(self.key),'-m',str(self.archive),'-t','new private signing pass'],check=True,capture_output=True)
  gh=self.bin/'gh';gh.write_text('''#!/usr/bin/env python3
import sys,os,json,shutil,pathlib
a=sys.argv[1:];root=pathlib.Path(os.environ['FAKE_ASSETS'])
if a[:2]==['release','download']:
 name=a[a.index('-p')+1];out=pathlib.Path(a[a.index('-D')+1]);out.mkdir(parents=True,exist_ok=True);shutil.copyfile(root/name,out/name)
elif a[:2]==['release','upload']:
 assert '--clobber' not in a
 files=a[a.index('-R')+2:]
 for item in files:
  source=pathlib.Path(item);assert not (root/source.name).exists();shutil.copyfile(source,root/source.name)
 with open(os.environ['FAKE_CALLS'],'a') as log:log.write(json.dumps(files)+'\\n')
else:raise SystemExit('unexpected gh command: '+repr(a))
''');gh.chmod(0o700)
 def tearDown(self):self.temp.cleanup()
 def existing(self):
  shutil.copyfile(self.archive,self.remote/self.name)
  subprocess.run(['minisign','-S','-s',str(self.key),'-m',str(self.remote/self.name),'-t','original published signature'],check=True,capture_output=True)
 def run_case(self):
  src=SCRIPT.read_text();verify=src[src.index('verify_release() {'):src.index('finish_release() {')]
  start=src.index('  if [ -n "$native_version" ]; then\n    ensure_release');end=src.index('\n  # Components before the channel',start);block=src[start:end]
  prefix='''set -euo pipefail
dir="$WORK";repo=example/cutex;native_version=0.167.0-r60;pub="$(sed -n 2p "$PUBKEY")"
ensure_release() { :; }
release_assets() { find "$FAKE_ASSETS" -maxdepth 1 -type f -printf '%f\\n'; }
do_gh() { gh "$@"; }
finish_release() { local tag="$1";shift 2;verify_release "$tag" "$@"; }
'''
  env={**os.environ,'WORK':str(self.work),'PUBKEY':str(self.pub),'FAKE_ASSETS':str(self.remote),'FAKE_CALLS':str(self.calls),'PATH':str(self.bin)+':'+os.environ['PATH']}
  return subprocess.run(['bash','-c',prefix+verify+'\n'+block],env=env,capture_output=True,text=True)
 def test_existing_payload_and_original_signature_are_untouched(self):
  self.existing();before={p.name:p.read_bytes() for p in self.remote.iterdir()};self.assertNotEqual(before[self.name+'.minisig'],Path(str(self.archive)+'.minisig').read_bytes())
  r=self.run_case();self.assertEqual(r.returncode,0,r.stderr);self.assertFalse(self.calls.exists());self.assertEqual(before,{p.name:p.read_bytes() for p in self.remote.iterdir()})
 def test_new_identity_uploads_without_clobber(self):
  r=self.run_case();self.assertEqual(r.returncode,0,r.stderr);self.assertEqual(len(json.loads(self.calls.read_text())),2)
 def test_only_missing_signature_is_uploaded(self):
  shutil.copyfile(self.archive,self.remote/self.name);r=self.run_case();self.assertEqual(r.returncode,0,r.stderr);self.assertEqual(json.loads(self.calls.read_text()),[str(self.archive)+'.minisig'])
 def test_only_missing_archive_is_uploaded_and_old_signature_kept(self):
  self.existing();signature=(self.remote/(self.name+'.minisig')).read_bytes();(self.remote/self.name).unlink();r=self.run_case();self.assertEqual(r.returncode,0,r.stderr);self.assertEqual(json.loads(self.calls.read_text()),[str(self.archive)]);self.assertEqual((self.remote/(self.name+'.minisig')).read_bytes(),signature)
 def test_different_existing_payload_aborts_without_upload(self):
  self.existing();(self.remote/self.name).write_bytes(b'different immutable bytes');r=self.run_case();self.assertNotEqual(r.returncode,0);self.assertFalse(self.calls.exists());self.assertEqual((self.remote/self.name).read_bytes(),b'different immutable bytes')
 def test_invalid_existing_signature_aborts_without_upload(self):
  self.existing();(self.remote/(self.name+'.minisig')).write_text('invalid');r=self.run_case();self.assertNotEqual(r.returncode,0);self.assertFalse(self.calls.exists())

if __name__=='__main__':unittest.main()
