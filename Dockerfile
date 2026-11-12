# MES-Intelligence: the central MES (and simulated machines for demos).
FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 MES_DB=/data/mes.sqlite3
WORKDIR /app

COPY requirements.txt pyproject.toml README.md ./
COPY src ./src
RUN pip install --no-cache-dir -r requirements.txt && pip install --no-cache-dir . \
    && useradd --create-home mes && mkdir /data && chown mes /data
USER mes

CMD ["python", "-m", "mes", "run"]
