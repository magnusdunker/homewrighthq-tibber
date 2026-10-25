# Tibber for HomewrightHQ

An external integration for [HomewrightHQ](https://github.com/magnusdunker/homewrighthq) for Tibber: electricity prices for today and tomorrow, consumption and cost, and live power from Tibber Pulse.

## Installation

In HomewrightHQ: **Settings → Integrations → From GitHub**. Paste

```
https://github.com/magnusdunker/homewrighthq-tibber
```

and press **Install**. Then create a personal access token on [developer.tibber.com](https://developer.tibber.com) (log in with your Tibber account), add **Tibber** under **Add integration**, and paste the token.

## What you get

Each home on the account becomes a device with:

- **Elpris** (electricity price): the current price including taxes in currency per kWh (e.g. `DKK/kWh`). The attributes `today` and `tomorrow` have today's and tomorrow's prices as lists of `{start, end, price}`, and `tomorrow_valid` says whether tomorrow's prices have arrived. This is the same shape the Energy page reads, so the price shows up there on its own. Also `energy`, `tax` and `level` for the current price and the day's `min_price`, `max_price` and `avg_price`.
- **Prisniveau** (price level): Tibber's level for the current price: `very_cheap`, `cheap`, `normal`, `expensive` or `very_expensive`.
- **Gennemsnitspris i dag** and **i morgen** (average price today and tomorrow), with the day's prices in the attribute `prices` and the start of the cheapest hour in `cheapest_start`. Tomorrow is `unknown` until Tibber has the prices, usually around 13:00.
- **Forbrug sidste time** (consumption last hour, kWh) with the cost as an attribute, **Forbrug i dag** (consumption today, kWh) and **Udgift i dag** (cost today, in the account's currency). Tibber gets meter data with a delay, so the latest hours are often missing.

A home with Tibber Pulse (or Watty) also gets live data over Tibber's websocket:

- **Effekt** (power, W) with minimum, maximum, average, voltage and current per phase and signal strength as attributes
- **Forbrug i dag (Pulse)** (consumption today, kWh), which counts up from midnight
- **Målerstand** (meter reading, kWh), which can be used on the Energy page
- **Produktion** (production, W), disabled by default; enable it if you have solar panels

The live sensors update at most every five seconds. If the connection drops, they become unavailable, and the integration reconnects after 5 seconds and waits twice as long for each attempt, up to 5 minutes.

## Prices per quarter hour

Under **Options** on the integration, the price resolution can be set to one price per quarter hour. Then `today` and `tomorrow` have 96 prices per day instead of 24. The default is one price per hour.

## When data is fetched

Prices are fetched at startup, when the day ends, every quarter hour from 13:00 until tomorrow's prices have arrived, and otherwise every six hours. The sensors follow the clock every minute, so the price changes every hour or quarter hour without a new request. Consumption for the last 48 hours is fetched once an hour. If Tibber answers 429 (too many requests), the integration pauses for 15 minutes or as long as Tibber asks.

If Tibber rejects the token, the integration creates a repair issue, and you can authorize the integration again with a new token.

## Not tested

The integration is written from Tibber's API as it is described in Home Assistant's Tibber integration and pyTibber, and tested against a fake Tibber with responses shaped like the real ones. Tibber's own documentation on developer.tibber.com could not be read during development. These things are not checked against the real Tibber:

- That `priceInfo(resolution: QUARTER_HOURLY)` exists and gives 96 prices per day. Without the option, the query is sent without an argument, which Tibber has always been able to answer.
- How a wrong or revoked token is answered. The integration counts HTTP 401 and 403, the error code `UNAUTHENTICATED` and error texts with "invalid token" or "Context creation failed" as a rejected token.
- Tibber's limit on the number of requests, and whether it is sent as 429 with `Retry-After`.
- The fields in the query for account and home: `userId`, `name`, `login`, `websocketSubscriptionUrl`, `appNickname`, `address.address1`, `features.realTimeConsumptionEnabled`.
- That `consumption(resolution: HOURLY, last: 48)` gives `nodes` with `from`, `to`, `consumption`, `cost`, `unitPrice` and `currency`, and that hours without a reading have `consumption: null`.
- That the websocket speaks `graphql-transport-ws` with the token as `{"token": ...}` in `connection_init`, and that a `subscribe` with the home's id as message id is enough for several homes on one connection.
- The fields in `liveMeasurement`, especially `lastMeterConsumption`, `powerProduction`, `voltagePhase1-3`, `currentL1-3` and `signalStrength`, which depend on the meter.
- That `accumulatedConsumption` resets at midnight.
- How long Tibber Pulse can be quiet before the connection should be restarted. The integration reconnects after 90 seconds without data.

Consumption is not added to long-term statistics (`statistics/import`), and Tibber's app features (car charging, heat pump) are not included.

## Development

Built in Rust on the `thehome-plugin` SDK from [homewright-example-integration-rust](https://github.com/magnusdunker/homewrighthq-example-integration-rust).

- `src/api.rs`: Tibber's GraphQL API and the queries
- `src/model.rs`: Tibber's data and how it becomes states, and the messages on the websocket
- `src/lib.rs`: the connection to HomewrightHQ, fetching and the live connection
- `tests/tibber.rs`: tests against a fake Tibber with the responses in `tests/fixtures`, a local websocket and a fake HomewrightHQ in `tests/host`

```
cargo test
cargo build --release
```

A new version is released with a tag, e.g. `git tag v0.1.0 && git push origin v0.1.0`. The workflow builds for Windows, Linux (x86_64 and arm64, static musl) and macOS and puts the binaries and `homewright.json` on the release.

## License

MIT
