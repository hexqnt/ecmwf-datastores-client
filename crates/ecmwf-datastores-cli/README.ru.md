# ecmwf-datastores CLI

[🇺🇸 English](./README.md) · [🇷🇺 Русский](./README.ru.md)

Утилита командной строки для воспроизводимого получения данных ECMWF. Планирует и восстанавливает задания, скачивает и собирает результаты, формирует отчёты о времени выполнения.

Структурированные секции `[time]` и `[bbox]` поддерживаются для:

- `reanalysis-era5-pressure-levels`
- `reanalysis-era5-single-levels`
- `reanalysis-era5-land`
- `derived-era5-single-levels-daily-statistics`
- `reanalysis-era5-complete`
- месячных средних ERA5-Land, single-level и pressure-level

Для остальных коллекций укажите точные поля CDS в `[request]`, без структурированных секций `[time]` и `[bbox]`.

## Установка

```sh
cargo install --git https://github.com/hexqnt/ecmwf-datastores-client.git \
  --package ecmwf-datastores-cli
```

Из клонированного репозитория:

```sh
cargo install --path crates/ecmwf-datastores-cli
```

Обе команды устанавливают исполняемый файл `ecmwf-datastores`.

## Конфигурация

Начните с [примеров конфигурации](./examples/). Даты и время указываются в кавычках. Для bbox используются пары `lat` и `lon` в формате `[min, max]`.

Секция `[time]` задаёт включительный диапазон дат. Остальные поля формы CDS помещаются в `[request]`; не дублируйте пространственные и временные поля, добавляемые планировщиком.

Планировщик разбивает известные оси по лимитам и стоимости провайдера, а при их недоступности — по локальной оценке. Значения по умолчанию: `max_items = 100000` и `max_requests = 1000`; они меняются в `[split]`. Неизвестные многозначные поля и сырые диапазоны сохраняются, но не учитываются в оценке числа элементов.

Credentials загружаются в порядке, заданном библиотекой. Явный файл выбирается через `--credentials PATH`.

## Команды

```sh
ecmwf-datastores plan request.toml
ecmwf-datastores constraints request.toml
ecmwf-datastores retrieve request.toml --report report.json
ecmwf-datastores resume JOB_ID output.nc
```

- `plan` выводит запросы без их отправки.
- `constraints` выводит разрешённые сервером значения; успех не гарантирует возможность отправки.
- `retrieve` отправляет план, скачивает и собирает результаты.
- `resume` продолжает одно известное задание.

Прогресс выводится в стандартный поток ошибок и настраивается через `--progress auto|always|never`. Для замены существующего файла нужен `--overwrite`.

## Выполнение и сборка

По умолчанию одновременно активны два серверных задания и одна загрузка. Лимиты можно изменить:

```toml
[execution]
max_active_jobs = 2
max_concurrent_downloads = 1
```

Части GRIB объединяются в порядке плана. Переменные NetCDF объединяются по именованным одномерным координатам; конфликтующие пересечения приводят к ошибке. Для сборки на диске должны помещаться все части и итоговый файл. После ошибки части сохраняются и могут быть собраны вручную:

```sh
ecmwf-datastores assemble result.nc .result.part-1.nc .result.part-2.nc
```

Явные архивы нельзя собрать из частей; при наличии поля используйте `download_format = "unarchived"`. Поддержка NetCDF включена стандартной feature `netcdf-assembly`. Для CLI только с GRIB укажите `--no-default-features`.

## Отчёты и восстановление

`--report PATH` создаёт JSON-отчёт и журнал `PATH.events.jsonl`. ID задания синхронно записывается сразу после отправки, поэтому прерванные задания можно восстановить. Итоговый отчёт содержит запросы, размеры файлов, время этапов, скорость и сохранённые части. `submission_unknown` означает, что сервер мог принять запрос, но не вернул ID задания.

## Импорт кода CDS

Сохраните фрагмент из **Show API request code** и выполните:

```sh
ecmwf-datastores import-python cds-request.py --output request.toml
```

Импортёр читает литералы Python, не исполняя исходный код. Полученный TOML можно планировать, проверять и загружать обычным способом.
