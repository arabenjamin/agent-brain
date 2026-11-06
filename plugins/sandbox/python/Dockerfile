# Code execution sandbox for the `execute_code` tool.
#
# Holds no credentials and mounts none of the brain's volumes. Compose runs it
# on an `internal: true` network, so submitted code has no route off the host —
# which is also why pip runs here at build time and never at run time.
FROM python:3.13-slim

# Compute libraries submitted code may import. sympy is the one that matters
# most: symbolic algebra is where a language model's prose arithmetic fails
# worst and an exact solver wins outright. scipy and scikit-learn are the
# ML Lab's lesson exercises and optimizers (ML_LAB_PLAN.md). cookiecutter
# renders the plugin template for the developer plugin's scaffold_plugin
# (CODEBASE_SPLIT_PLAN Phase 2 step 5): here because it evaluates answers a
# model wrote as Jinja, and this is the place with no network and no secrets.
RUN pip install --no-cache-dir \
        numpy==2.2.1 \
        sympy==1.13.3 \
        pandas==2.2.3 \
        scipy==1.15.1 \
        scikit-learn==1.6.1 \
        cookiecutter==2.7.1

# Not UID 1000: RLIMIT_NPROC is counted per real UID across the whole kernel,
# so sharing the host user's UID (about 2,000 threads) meant a run could start
# no child process or thread at all (found 2026-11-06). Not 10001 either: that
# is the Rust sandbox's, whose builds would spend this sandbox's allowance.
RUN useradd -m -u 10002 sandbox

COPY server.py /usr/local/bin/sandbox-server.py

USER sandbox
WORKDIR /tmp

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD python3 -c "import urllib.request;urllib.request.urlopen('http://localhost:8000/health').read()"

CMD ["python3", "/usr/local/bin/sandbox-server.py"]
