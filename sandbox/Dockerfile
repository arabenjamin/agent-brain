# Code execution sandbox for the `execute_code` tool.
#
# Holds no credentials and mounts none of the brain's volumes. Compose runs it
# on an `internal: true` network, so submitted code has no route off the host —
# which is also why pip runs here at build time and never at run time.
FROM python:3.13-slim

# Compute libraries submitted code may import. sympy is the one that matters
# most: symbolic algebra is where a language model's prose arithmetic fails
# worst and an exact solver wins outright.
RUN pip install --no-cache-dir \
        numpy==2.2.1 \
        sympy==1.13.3 \
        pandas==2.2.3

RUN useradd -m -u 1000 sandbox

COPY server.py /usr/local/bin/sandbox-server.py

USER sandbox
WORKDIR /tmp

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD python3 -c "import urllib.request;urllib.request.urlopen('http://localhost:8000/health').read()"

CMD ["python3", "/usr/local/bin/sandbox-server.py"]
