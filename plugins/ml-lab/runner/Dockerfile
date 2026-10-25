# The ML Lab runner (ML_LAB_PLAN.md Track B): the Squares in Squares store,
# the overnight optimizer, the scorer (Kaggle's own metric) and the submitter;
# and the Playground Series pipeline (tabular ML, ML_LAB_PLAN.md Track A).
#
# Runs only the code in this directory, never code from a request, which is
# why it may reach the internet (Kaggle) where the sandboxes may not.
FROM python:3.13-slim

# LightGBM and XGBoost need the OpenMP runtime, which the slim image lacks.
RUN apt-get update && apt-get install -y --no-install-recommends libgomp1 \
    && rm -rf /var/lib/apt/lists/*
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir --root-user-action=ignore -r /tmp/requirements.txt

RUN useradd -m -u 1000 runner
COPY squares /app/squares
COPY playground /app/playground
ENV PYTHONPATH=/app PYTHONUNBUFFERED=1 NUMBA_CACHE_DIR=/tmp/numba \
    OPENBLAS_NUM_THREADS=1 OMP_NUM_THREADS=1
USER runner
WORKDIR /app
EXPOSE 8000
CMD ["python", "-m", "squares.main"]
