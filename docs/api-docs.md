# просмотр списка файлов

```
curl -H "Authorization: OAuth $TOKEN" \
  "https://cloud-api.yandex.net/v1/disk/resources?path=disk:/"
```

# загрузка файла

```
curl -H "Authorization: OAuth $TOKEN" \
  "https://cloud-api.yandex.net/v1/disk/resources/download?path=disk:/hello.txt"
wget "<href_из_ответа>"
```

wget мне чёт forbidden выдаёт, короче, разберись как-нибудь, как это сделать
