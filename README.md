# ESP32-C6 Stock Ticker

A `no_std` quote ticker for the **Waveshare ESP32-C6-LCD-1.47** board.

All quotes come from `http://hq.sinajs.cn` via plain HTTP batch request. 
Pre-/after-market values are picked up automatically when the feed
publishes them. Data refresh every 30 s while the display cycles through the
tickers.
