FROM python:3.13-slim

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY . .

ENV PAPER_TRADE=true
ENV LOG_LEVEL=INFO
ENV BTC_DB_PATH=/data/btc_trades.db

EXPOSE 8080
VOLUME ["/data"]

ENTRYPOINT ["python", "btc_main.py"]
