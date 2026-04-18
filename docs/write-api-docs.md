Наблюдения по реальному API:

- для вызовов `cloud-api.yandex.net/v1/disk/...` нужен `Authorization: OAuth <token>`
- `href`, который возвращают `/resources/upload` и `/resources/download`, используется уже без OAuth-заголовка
- download `href` может отвечать redirect-ом, так что клиент должен уметь следовать редиректу
- range-чтение по download `href` работает, если клиент проходит redirect

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
