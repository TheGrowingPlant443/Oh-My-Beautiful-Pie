import json
import yaml
import requests

def unsafe(data):
    # ruleid: omp-quality.python.eval
    eval(data)
    # ruleid: omp-quality.python.exec
    exec(data)
    # ruleid: omp-quality.python.unsafe-yaml
    yaml.load(data)
    # ruleid: omp-quality.python.unsafe-yaml
    yaml.load(data, Loader=yaml.Loader)
    # ruleid: omp-quality.python.tls-verification-disabled
    requests.get('https://example.invalid', verify=False)

def safe(data):
    # ok: omp-quality.python.eval
    json.loads(data)
    # ok: omp-quality.python.unsafe-yaml
    yaml.safe_load(data)
    # ok: omp-quality.python.unsafe-yaml
    yaml.load(data, Loader=yaml.SafeLoader)
    # ok: omp-quality.python.unsafe-yaml
    yaml.load(data, yaml.SafeLoader)
    # ok: omp-quality.python.tls-verification-disabled
    requests.get('https://example.invalid')
    # ok: omp-quality.python.tls-verification-disabled
    requests.get('https://example.invalid', verify=True)
