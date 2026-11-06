"""Pre-merge/CI entry point. Configure the Rust toolchain in the caller's environment."""
import argparse, os, pathlib, subprocess, sys

def main():
    p=argparse.ArgumentParser(); p.add_argument('--scratch',type=pathlib.Path,required=True); p.add_argument('--revision',default='HEAD'); p.add_argument('--concurrency',default='1,2,4'); p.add_argument('--repeats',default='3'); p.add_argument('--scales',default='0.1,1'); p.add_argument('--histories'); p.add_argument('--anchor'); a=p.parse_args()
    a.scratch=a.scratch.resolve(); a.scratch.mkdir(parents=True,exist_ok=True)
    os.environ.setdefault('CARGO_TARGET_DIR',str(a.scratch/'target'))
    here=pathlib.Path(__file__).resolve().parent
    subprocess.run([sys.executable,str(here/'build.py'),'--scratch',str(a.scratch/'build'),'--revision',a.revision],check=True)
    exe=pathlib.Path(os.environ['CARGO_TARGET_DIR'])/'debug'/('cutex.exe' if os.name=='nt' else 'cutex')
    args=[sys.executable,str(here/'run.py'),'--exe',str(exe),'--output',str(a.scratch/'results'),'--concurrency',a.concurrency,'--repeats',a.repeats,'--scales',a.scales]
    if a.histories: args+=['--histories',a.histories]
    if a.anchor: args+=['--anchor',a.anchor]
    return subprocess.run(args).returncode
if __name__=='__main__': sys.exit(main())
