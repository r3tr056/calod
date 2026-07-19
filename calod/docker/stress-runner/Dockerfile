FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

WORKDIR /runner

COPY docker/stress-runner/requirements.txt ./requirements.txt

RUN pip install --no-cache-dir -r requirements.txt

COPY scripts/stress_runner.py /runner/stress_runner.py

RUN useradd --system --uid 10002 --create-home runner \
    && chown -R runner:runner /runner

USER runner

ENTRYPOINT ["python", "/runner/stress_runner.py"]
