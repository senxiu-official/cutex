import sys,unittest,json
from pathlib import Path
import runtime_prune_protection as p
from test_runtime_prune_protection import ProtectionTests,write

class AbsenceTests(ProtectionTests):
 def refresh(self):
  write(self.op/'component-prepared.json',self.prepared)
  self.record['componentPreparedSha256']=p.digest(self.op/'component-prepared.json')
  self.save()
 def absent(self,name):
  self.backups.pop('runtime/'+name+'/deployment.json')
  self.protected[name+'-runtime']={'exists':False,'sha256':None}
  path=self.op/'originals'/self.originals[str(self.state_path)]['file']
  state=json.loads(path.read_text());state.pop(name+'-runtime');write(path,state)
  self.originals[str(self.state_path)]['sha256']=p.digest(path)
  self.protected['state']['sha256']=p.digest(path)
  self.refresh()
 def test_real_no_dsh_no_kimi_shape(self):
  self.absent('dsh');self.absent('kimi')
  result=p.build_index(self.op,self.active_executable)
  self.assertFalse(any('/before/dsh/' in Path(x).as_posix() or '/before/kimi/' in Path(x).as_posix() for x in result['protectedPaths']))
 def test_absent_hash_refuses(self):
  self.absent('dsh');self.protected['dsh-runtime']['sha256']='a'*64;self.refresh()
  with self.assertRaisesRegex(p.ProtectionError,'absent before deployment has a hash'):p.build_index(self.op,self.active_executable)
 def test_absent_backup_refuses(self):
  self.protected['dsh-runtime']={'exists':False,'sha256':None};self.refresh()
  with self.assertRaisesRegex(p.ProtectionError,'absent before deployment has a backup'):p.build_index(self.op,self.active_executable)
 def test_absent_selected_refuses(self):
  self.backups.pop('runtime/dsh/deployment.json');self.protected['dsh-runtime']={'exists':False,'sha256':None};self.refresh()
  with self.assertRaisesRegex(p.ProtectionError,'absent before deployment has selected component'):p.build_index(self.op,self.active_executable)
 def test_missing_absence_proof_refuses(self):
  self.absent('kimi');self.protected.pop('kimi-runtime');self.refresh()
  with self.assertRaises(p.ProtectionError):p.build_index(self.op,self.active_executable)

if __name__=='__main__':
 suite=unittest.TestSuite(AbsenceTests(name) for name in ['test_real_no_dsh_no_kimi_shape','test_absent_hash_refuses','test_absent_backup_refuses','test_absent_selected_refuses','test_missing_absence_proof_refuses'])
 result=unittest.TextTestRunner(verbosity=2).run(suite);sys.exit(not result.wasSuccessful())
