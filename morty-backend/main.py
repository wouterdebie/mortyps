import datetime
import random
import time
import pendulum

from flask import Flask, render_template, request, send_from_directory
from google.cloud import datastore
import math

client = datastore.Client()

app = Flask(__name__)


def store_location(source, location):
    """Store a location in Datastore."""

    parent_key = client.key('source', source)
    parent_entity = client.get(parent_key)

    if parent_entity is None:
        parent_entity = datastore.Entity(key=parent_key)

    parent_entity.update({
        'id': source,
    })
    client.put(parent_entity)

    key = client.key('location', parent=parent_key)
    entity = datastore.Entity(key=key, exclude_from_indexes=['expiry_timestamp'])
    lat = location['latitude']
    lon = location['longitude']
    hdop = location['hdop']
    charging = location['charging']
    battery_voltage = location['battery_voltage']


    time_now = pendulum.now()
    expiry_timestamp = time_now.add(days=30)

    entity.update({
        'latitude': None if lat == None else float(lat),
        'longitude': None if lon == None else float(lon),
        'hdop': None if hdop == None else float(hdop),
        'timestamp': int(location['timestamp']),
        'utc': int(location['utc']),
        'fix_quality': int(location['fix_quality']),
        'satellites': int(location['satellites']),
        'expiry_timestamp': expiry_timestamp.timestamp(),
        'uid': location['uid'],
        'charging': charging,
        'battery_voltage': float(battery_voltage),
    })
    client.put(entity)

def fetch_locations(source=None):
    """Fetch locations from Datastore."""
    query = client.query(kind='location')
    query.order = ['-timestamp']
    if source:
        query.add_filter('source', '=', source)
    return list(query.fetch())

def fetch_sources():
    """Fetch distinct sources from Datastore."""
    query = client.query(kind='source')
    return list(query.fetch())

@app.route('/api/v1/sources')
def sources():
    sources = [s for s in fetch_sources()]
    return sources

@app.route('/api/v1/source/<source>/location', methods=['POST'])
def post_locastion(source):
    location = request.get_json()
    store_location(source, location)
    return {'status': 'ok'}

@app.route('/api/v1/source/<source>/locations/last_seen/<limit>', methods=['GET'])
def source_locations_last_seen(source, limit):
    parent = client.key('source', source)
    query = client.query(kind='location', ancestor=parent)
    query.add_filter('fix_quality', '!=', 0)
    query.order = ["fix_quality", "-timestamp"]
    return list(query.fetch(limit=int(limit)))

@app.route('/api/v1/source/<source>/locations/last_ping/<limit>', methods=['GET'])
def source_locations_last_ping(source, limit):
    parent = client.key('source', source)
    query = client.query(kind='location', ancestor=parent)
    query.order = ["-timestamp"]
    return list(query.fetch(limit=int(limit)))

@app.route('/api/v1/sources/locations/last_seen', methods=['GET'])
def last_locations():
    sources = fetch_sources()
    for source in sources:
        source['locations'] = source_locations_last_seen(source["id"], 100)
    return sources

@app.route('/api/v1/source/<source>/location/last_seen', methods=['GET'])
def last_location(source):
    parent = client.key('source', source)
    query = client.query(kind='location', ancestor=parent)
    query.add_filter('fix_quality', '!=', 0)
    query.order = ["fix_quality", "-timestamp"]
    result = list(query.fetch(limit=1))
    if len(result) == 0:
        return {}

    return result[0]

@app.route('/api/v1/source/<source>/location/last_ping', methods=['GET'])
def last_ping(source):
    parent = client.key('source', source)
    query = client.query(kind='location', ancestor=parent)
    query.order = ["-timestamp"]
    return list(query.fetch(limit=1))

@app.route('/api/v1/source/<source>', methods=['PUT'])
def update_source(source):
    properties = request.get_json()
    entity = client.get(client.key('source', source))
    entity.update(properties)
    client.put(entity)
    return entity

@app.route('/')
def root():
    return send_from_directory('static', "index.html")



if __name__ == '__main__':
    # This is used when running locally only. When deploying to Google App
    # Engine, a webserver process such as Gunicorn will serve the app. This
    # can be configured by adding an `entrypoint` to app.yaml.
    # Flask's development server will automatically serve static files in
    # the "static" directory. See:
    # http://flask.pocoo.org/docs/1.0/quickstart/#static-files. Once deployed,
    # App Engine itself will serve those files as configured in app.yaml.
    app.run(host='127.0.0.1', port=8080, debug=True)
