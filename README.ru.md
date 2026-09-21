# ecmwf-datastores-client

[![CI](https://github.com/hexqnt/ecmwf-datastores-client/actions/workflows/ci.yml/badge.svg)](https://github.com/hexqnt/ecmwf-datastores-client/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/ecmwf-datastores-client.svg)](https://crates.io/crates/ecmwf-datastores-client) [![docs.rs](https://docs.rs/ecmwf-datastores-client/badge.svg)](https://docs.rs/ecmwf-datastores-client)

[🇺🇸 English](./README.md) · [🇷🇺 Русский](./README.ru.md)

Неофициальный асинхронный Rust-клиент для ECMWF Data Stores API (CDS, ADS и EWDS). Поддерживает работу с каталогом, заданиями на получение данных, профилями пользователей и планирование крупных запросов ERA5.

## Установка

```sh
cargo add ecmwf-datastores-client
```

## Возможности

- Типизированные идентификаторы, статусы заданий, экстенты и входные данные планировщика.
- Отправка заданий, проверка статуса, восстановление по ID и удаление.
- Поиск по каталогу, формы и ограничения коллекций, пагинация.
- Загрузка результатов в память, поток или файл.
- Локальное планирование запросов с уточнением по оценке провайдера.

## Конфигурация

Клиент принимает явные endpoint и API-ключ. Включённая по умолчанию feature `discovery` также загружает credentials из `ECMWF_DATASTORES_URL`/`ECMWF_DATASTORES_KEY`, `~/.ecmwfdatastoresrc` или `~/.cdsapirc`:

```rust
use ecmwf_datastores_client::{Client, Credentials, error::Result};

#[cfg(feature = "discovery")]
fn client() -> Result<Client> {
    Client::from_credentials(Credentials::discover()?)
}
```

Если поиск credentials не нужен, укажите `default-features = false`. `Credentials::from_file()` читает YAML с полем `url` и необязательным `key`.

## Пример

```rust
use ecmwf_datastores_client::{
    Client, CollectionId, Credentials, ExistingTarget, Selection, error::Result,
};
use serde_json::json;

#[cfg(feature = "discovery")]
async fn download_example() -> Result<()> {
    let client = Client::from_credentials(Credentials::from_cdsapirc()?)?;
    let collection = CollectionId::parse("reanalysis-era5-single-levels")?;
    let selection = Selection::try_from(json!({
        "product_type": ["reanalysis"],
        "variable": ["2m_temperature"],
        "year": ["2023"],
        "month": ["01"],
        "day": ["01"],
        "time": ["00:00"],
        "data_format": "netcdf"
    }))?;

    let job = client.submit(&collection, &selection).await?;
    let job_id = job.id().clone();
    let results = client.job(&job_id)?.wait_for_results().await?;
    results.asset().download_to("era5.nc", ExistingTarget::Error).await?;
    Ok(())
}
```

Сохраните `job_id.to_string()`, чтобы восстановить задание после перезапуска. `wait_for_results()` ожидает завершения, а `status()` делает один запрос. Для удаления используйте `Job::delete()` или `Client::delete_jobs()`.

Полезные API:

- `Asset::bytes()`, `byte_stream()` и `download_to()` — получение результата.
- `Job::details()` — типизированные данные задания, `Client::receipt_details()` — типизированная квитанция.
- `Client::collection_form()` и `collection_constraints()` — поля запроса.
- `Client::builder().wait_timeout()` — изменение стандартного лимита ожидания в 24 часа.

## Планирование запросов

Модуль `planning` локально разбивает крупные запросы ERA5. `Planner::plan()` при возможности уточняет разбиение через costing endpoint провайдера, но не отправляет задания. `plan_locally()` строит план без I/O, а `refine_with_costs()` уточняет существующий.

```rust
use ecmwf_datastores_client::{Client, planning::{Planner, PlanningRequest}};

async fn planning_example(
    client: &Client,
    request: &PlanningRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let outcome = Planner::new().plan(client, request).await?;
    for part in outcome.plan().requests() {
        client.submit(outcome.plan().dataset(), part.selection()).await?;
    }
    Ok(())
}
```

## Примечания

- `Selection` — JSON-объект; поля конкретного датасета проверяет сервис.
- Загрузка файла продолжается в рамках того же вызова `download_to()`, если это поддерживает сервер. При отмене временный файл удаляется.
- Open Data API, IFS и AIFS не входят в область библиотеки.

Для заданий в TOML, отображения прогресса, сборки частей и импорта Python-фрагментов CDS используйте отдельный пакет [`ecmwf-datastores-cli`](https://github.com/hexqnt/ecmwf-datastores-client/tree/main/crates/ecmwf-datastores-cli).

## Разработка

Каталог бенчмарков, сравнение baseline, построение flamegraph и команды Linux
`perf` описаны в [руководстве по замерам производительности](perf/README.md).
