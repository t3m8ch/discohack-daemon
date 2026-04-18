Наблюдения по реальному API:

- для вызовов `cloud-api.yandex.net/v1/disk/...` нужен `Authorization: OAuth <token>`
- `href`, который возвращают `/resources/upload` и `/resources/download`, используется уже без OAuth-заголовка
- download `href` может отвечать redirect-ом, так что клиент должен уметь следовать редиректу
- range-чтение по download `href` работает, если клиент проходит redirect
- для offline-first слоя полезно читать revision/etag-подобное поле ресурса и сохранять его как `remote_version`
- клиент не должен полагаться на `mtime/size` как на единственный conflict detector
- `move`/`delete`/`mkdir` могут возвращать operation-style ответы, так что sync worker должен быть готов к eventual completion, а не только к мгновенному success path

# Создание папки

curl -X PUT -H "Authorization: OAuth $TOKEN" \
  "https://cloud-api.yandex.net/v1/disk/resources?path=disk:/test-dir"

# Удаление папки или файла

curl -X DELETE -H "Authorization: OAuth $TOKEN" \
  "https://cloud-api.yandex.net/v1/disk/resources?path=disk:/test-dir&permanently=true"

# Загрузка файла

## Получить URL для загрузки

curl -H "Authorization: OAuth $TOKEN" \
  "https://cloud-api.yandex.net/v1/disk/resources/upload?path=disk:/hello.txt&overwrite=true"

## Загрузить файл по полученной ссылке

curl -T ./hello.txt "<href_из_предыдущего_ответа>"

# Безопасный baseline для conflict check

- перед upload нужно получить текущее metadata по пути
- если текущая `remote_version` не совпадает с последней синхронизированной локально, overwrite делать нельзя
- в таком случае локальная версия должна уходить в conflict copy с числовым суффиксом, а не затирать remote
