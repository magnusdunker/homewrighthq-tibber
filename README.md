# Tibber til HomewrightHQ

En ekstern integration til [HomewrightHQ](https://github.com/magnusdunker/homewrighthq) for Tibber: elpriser for i dag og i morgen, forbrug og udgift, og live-effekt fra Tibber Pulse.

## Installation

I HomewrightHQ: **Indstillinger → Integrationer → Fra GitHub**. Indsæt

```
https://github.com/magnusdunker/homewrighthq-tibber
```

og tryk **Installér**. Lav derefter en personlig adgangsnøgle på [developer.tibber.com](https://developer.tibber.com) (log ind med din Tibber-konto), tilføj **Tibber** under **Tilføj integration**, og indsæt nøglen.

## Hvad du får

Hvert hjem på kontoen bliver en enhed med:

- **Elpris**: prisen nu inklusive afgifter i valuta pr. kWh (f.eks. `DKK/kWh`). Attributterne `today` og `tomorrow` har dagens og morgendagens priser som lister af `{start, end, price}`, og `tomorrow_valid` siger, om morgendagens priser er kommet. Det er samme form, som Energi-siden læser, så prisen vises der af sig selv. Desuden `energy`, `tax` og `level` for den aktuelle pris og dagens `min_price`, `max_price` og `avg_price`.
- **Prisniveau**: Tibbers niveau for prisen nu: `very_cheap`, `cheap`, `normal`, `expensive` eller `very_expensive`.
- **Gennemsnitspris i dag** og **i morgen**, med dagens priser i attributten `prices` og starten på den billigste time i `cheapest_start`. I morgen er `unknown`, til Tibber har priserne, normalt omkring kl. 13.
- **Forbrug sidste time** (kWh) med udgiften som attribut, **Forbrug i dag** (kWh) og **Udgift i dag** (i kontoens valuta). Tibber får målerdata med forsinkelse, så de seneste timer mangler ofte.

Et hjem med Tibber Pulse (eller Watty) får også live-data over Tibbers websocket:

- **Effekt** (W) med minimum, maksimum, gennemsnit, spænding og strøm pr. fase og signalstyrke som attributter
- **Forbrug i dag (Pulse)** (kWh), som tæller op fra midnat
- **Målerstand** (kWh), som kan bruges på Energi-siden
- **Produktion** (W), slået fra som standard; slå den til, hvis du har solceller

Live-sensorerne opdateres højst hvert femte sekund. Ryger forbindelsen, bliver de utilgængelige, og integrationen forbinder igen efter 5 sekunder og venter dobbelt så længe for hvert forsøg, dog højst 5 minutter.

## Priser pr. kvarter

Under **Indstillinger** på integrationen kan prisopløsningen sættes til en pris pr. kvarter. Så har `today` og `tomorrow` 96 priser pr. dag i stedet for 24. Standard er en pris pr. time.

## Hvornår der hentes

Priserne hentes ved start, når dagen er slut, hvert kvarter fra kl. 13, til morgendagens priser er kommet, og ellers hver sjette time. Sensorerne følger uret hvert minut, så prisen skifter ved hver time eller hvert kvarter uden en ny forespørgsel. Forbruget for de sidste 48 timer hentes en gang i timen. Svarer Tibber 429 (for mange forespørgsler), holder integrationen pause i 15 minutter eller så længe Tibber beder om.

Afviser Tibber nøglen, opretter integrationen et problem, og du kan godkende integrationen igen med en ny nøgle.

## Ikke afprøvet

Integrationen er skrevet ud fra Tibbers API, som det er beskrevet i Home Assistants Tibber-integration og pyTibber, og afprøvet mod en falsk Tibber med svar formet som de rigtige. Tibbers egen dokumentation på developer.tibber.com kunne ikke læses under udviklingen. Disse ting er ikke kontrolleret mod den rigtige Tibber:

- At `priceInfo(resolution: QUARTER_HOURLY)` findes og giver 96 priser pr. dag. Uden indstillingen sendes forespørgslen uden argument, som Tibber altid har kunnet besvare.
- Hvordan en forkert eller tilbagekaldt nøgle besvares. Integrationen regner HTTP 401 og 403, fejlkoden `UNAUTHENTICATED` og fejltekster med "invalid token" eller "Context creation failed" som en afvist nøgle.
- Tibbers grænse for antal forespørgsler, og om den sendes som 429 med `Retry-After`.
- Felterne i forespørgslen efter konto og hjem: `userId`, `name`, `login`, `websocketSubscriptionUrl`, `appNickname`, `address.address1`, `features.realTimeConsumptionEnabled`.
- At `consumption(resolution: HOURLY, last: 48)` giver `nodes` med `from`, `to`, `consumption`, `cost`, `unitPrice` og `currency`, og at timer uden måling har `consumption: null`.
- At websocketten taler `graphql-transport-ws` med nøglen som `{"token": ...}` i `connection_init`, og at en `subscribe` med hjemmets id som besked-id er nok til flere hjem på én forbindelse.
- Felterne i `liveMeasurement`, især `lastMeterConsumption`, `powerProduction`, `voltagePhase1-3`, `currentL1-3` og `signalStrength`, som afhænger af måleren.
- At `accumulatedConsumption` nulstilles ved midnat.
- Hvor længe Tibber Pulse kan være stille, før forbindelsen bør genstartes. Integrationen forbinder igen efter 90 sekunder uden data.

Forbruget lægges ikke i langtidsstatistikken (`statistics/import`), og Tibbers app-funktioner (opladning af bil, varmepumpe) er ikke med.

## Udvikling

Bygget i Rust på SDK'en `thehome-plugin` fra [homewright-example-integration-rust](https://github.com/magnusdunker/homewrighthq-example-integration-rust).

- `src/api.rs`: Tibbers GraphQL-API og forespørgslerne
- `src/model.rs`: Tibbers data og hvordan de bliver til tilstande, og beskederne på websocketten
- `src/lib.rs`: forbindelsen til HomewrightHQ, hentning og live-forbindelsen
- `tests/tibber.rs`: tests mod en falsk Tibber med svarene i `tests/fixtures`, en lokal websocket og en falsk HomewrightHQ i `tests/host`

```
cargo test
cargo build --release
```

En ny version udgives med et tag, f.eks. `git tag v0.1.0 && git push origin v0.1.0`. Workflowet bygger til Windows, Linux (x86_64 og arm64, statisk musl) og macOS og lægger binærerne og `homewright.json` på releasen.

## Licens

MIT
