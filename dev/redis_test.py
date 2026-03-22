import redis
import time
import pytest

def test_calod_server_redis_compatibility():
    try:
        r = redis.Redis(host='localhost', port=6379, db=0)
        r.ping()
        print("\nConnected to Calod server successfully!")

        # --- Connection Commands ---
        print("\n--- Testing Connection Commands ---")
        assert r.ping() == True, "PING failed"
        print("PING test passed")
        echo_message = "Hello Calod!"
        assert r.echo(echo_message).decode('utf-8') == echo_message, "ECHO failed"
        print("ECHO test passed")

        # --- String Commands ---
        print("\n--- Testing String Commands ---")
        assert r.set('string_key', 'string_value') == True, "SET failed"
        print("SET test passed")
        assert r.get('string_key').decode('utf-8') == 'string_value', "GET failed"
        print("GET test passed")
        assert r.append('string_key', '_append').decode('utf-8') == 25, "APPEND failed"
        assert r.strlen('string_key') == 25, "STRLEN failed"
        print("APPEND and STRLEN tests passed")
        assert r.getrange('string_key', 0, 5).decode('utf-8') == 'string', "GETRANGE failed"
        print("GETRANGE test passed")
        assert r.setrange('string_key', 6, 'RANGE').decode('utf-8') == 25, "SETRANGE failed"
        assert r.get('string_key').decode('utf-8') == 'stringRANGE_append', "SETRANGE verification failed"
        print("SETRANGE test passed")
        assert r.getset('string_key', 'new_string_value').decode('utf-8') == 'stringRANGE_append', "GETSET failed - old value incorrect"
        assert r.get('string_key').decode('utf-8') == 'new_string_value', "GETSET failed - set new value incorrect"
        print("GETSET test passed")
        r.set('mget_key1', 'value1')
        r.set('mget_key2', 'value2')
        mget_result = r.mget(['mget_key1', 'mget_key2', 'non_existent_key'])
        assert [val.decode('utf-8') if val else None for val in mget_result] == ['value1', 'value2', None], "MGET failed"
        print("MGET test passed")
        assert r.mset({'mset_key1': 'mset_value1', 'mset_key2': 'mset_value2'}) == True, "MSET failed"
        assert r.get('mset_key1').decode('utf-8') == 'mset_value1', "MSET verification failed for key1"
        assert r.get('mset_key2').decode('utf-8') == 'mset_value2', "MSET verification failed for key2"
        print("MSET test passed")
        assert r.incr('incr_key') == 1, "INCR (new key) failed"
        assert r.incr('incr_key') == 2, "INCR (existing key) failed"
        print("INCR test passed")
        assert r.decr('decr_key') == -1, "DECR (new key) failed"
        assert r.decr('decr_key') == -2, "DECR (existing key) failed"
        print("DECR test passed")
        assert r.incrby('incrby_key', 5) == 5, "INCRBY failed"
        assert r.incrby('incrby_key', 3) == 8, "INCRBY (again) failed"
        print("INCRBY test passed")
        assert r.decrby('decrby_key', 5) == -7, "DECRBY failed"
        assert r.decrby('decrby_key', 3) == -10, "DECRBY (again) failed"
        print("DECRBY test passed")
        assert abs(r.incrbyfloat('incrbyfloat_key', 1.5) - 1.5) < 0.0001, "INCRBYFLOAT failed"
        assert abs(r.incrbyfloat('incrbyfloat_key', 2.5) - 4.0) < 0.0001, "INCRBYFLOAT (again) failed"
        print("INCRBYFLOAT test passed")

        # --- Hash Commands ---
        print("\n--- Testing Hash Commands ---")
        assert r.hset('hash_key', 'field1', 'hash_value1') == 1, "HSET (new field) failed"
        assert r.hset('hash_key', 'field1', 'hash_value1_updated') == 0, "HSET (existing field) failed"
        assert r.hset('hash_key', mapping={'field2': 'hash_value2', 'field3': 'hash_value3'}) == 2, "HSET (multiple fields) failed"
        print("HSET test passed")
        assert r.hget('hash_key', 'field1').decode('utf-8') == 'hash_value1_updated', "HGET failed"
        print("HGET test passed")
        assert r.hdel('hash_key', 'field2') == 1, "HDEL (existing field) failed"
        assert r.hdel('hash_key', 'non_existent_field') == 0, "HDEL (non-existent field) failed"
        print("HDEL test passed")
        assert r.hexists('hash_key', 'field1') == True, "HEXISTS (existing field) failed"
        assert r.hexists('hash_key', 'field2') == False, "HEXISTS (non-existent field) failed"
        print("HEXISTS test passed")
        hgetall_result = r.hgetall('hash_key')
        decoded_hgetall = {k.decode('utf-8'): v.decode('utf-8') for k, v in hgetall_result.items()}
        assert decoded_hgetall == {'field1': 'hash_value1_updated', 'field3': 'hash_value3'}, "HGETALL failed"
        print("HGETALL test passed")
        assert r.hincrby('hincrby_hash_key', 'field1', 5) == 5, "HINCRBY (new field) failed"
        assert r.hincrby('hincrby_hash_key', 'field1', 3) == 8, "HINCRBY (existing field) failed"
        print("HINCRBY test passed")
        assert abs(r.hincrbyfloat('hincrbyfloat_hash_key', 'field1', 1.5) - 1.5) < 0.0001, "HINCRBYFLOAT (new field) failed"
        assert abs(r.hincrbyfloat('hincrbyfloat_hash_key', 'field1', 2.5) - 4.0) < 0.0001, "HINCRBYFLOAT (existing field) failed"
        print("HINCRBYFLOAT test passed")
        hkeys_result = r.hkeys('hash_key')
        assert sorted([k.decode('utf-8') for k in hkeys_result]) == sorted(['field1', 'field3']), "HKEYS failed"
        print("HKEYS test passed")
        assert r.hlen('hash_key') == 2, "HLEN failed"
        print("HLEN test passed")
        hmget_result = r.hmget('hash_key', ['field1', 'field3', 'non_existent_field'])
        assert [val.decode('utf-8') if val else None for val in hmget_result] == ['hash_value1_updated', 'hash_value3', None], "HMGET failed"
        print("HMGET test passed")
        assert r.hmset('hmset_hash_key', {'field1': 'hmset_value1', 'field2': 'hmset_value2'}) == True, "HMSET failed"
        assert r.hget('hmset_hash_key', 'field1').decode('utf-8') == 'hmset_value1', "HMSET verification failed for field1"
        assert r.hget('hmset_hash_key', 'field2').decode('utf-8') == 'hmset_value2', "HMSET verification failed for field2"
        print("HMSET test passed")
        assert r.hsetnx('hsetnx_hash_key', 'field1', 'hsetnx_value1') == True, "HSETNX (new field) failed"
        assert r.hsetnx('hsetnx_hash_key', 'field1', 'hsetnx_value1_attempt2') == False, "HSETNX (existing field) failed"
        assert r.hget('hsetnx_hash_key', 'field1').decode('utf-8') == 'hsetnx_value1', "HSETNX verification failed"
        print("HSETNX test passed")
        hvals_result = r.hvals('hash_key')
        assert sorted([v.decode('utf-8') for v in hvals_result]) == sorted(['hash_value1_updated', 'hash_value3']), "HVALS failed"
        print("HVALS test passed")


        # --- List Commands ---
        print("\n--- Testing List Commands ---")
        assert r.lpush('list_key', ['list_value1', 'list_value2']) == 2, "LPUSH failed"
        assert r.rpush('list_key', ['list_value3', 'list_value4']) == 4, "RPUSH failed"
        print("LPUSH and RPUSH tests passed")
        assert r.lpop('list_key').decode('utf-8') == 'list_value2', "LPOP failed"
        assert r.rpop('list_key').decode('utf-8') == 'list_value4', "RPOP failed"
        print("LPOP and RPOP tests passed")
        assert r.llen('list_key') == 2, "LLEN failed"
        print("LLEN test passed")
        lrange_result = r.lrange('list_key', 0, -1)
        assert [val.decode('utf-8') for val in lrange_result] == ['list_value1', 'list_value3'], "LRANGE failed"
        print("LRANGE test passed")
        assert r.lindex('list_key', 0).decode('utf-8') == 'list_value1', "LINDEX (valid index) failed"
        assert r.lindex('list_key', 10) == None, "LINDEX (out of range) failed"
        print("LINDEX test passed")
        assert r.linsert('list_key', 'BEFORE', 'list_value3', 'inserted_value_before').decode('utf-8') == '3', "LINSERT BEFORE failed"
        assert r.linsert('list_key', 'AFTER', 'list_value3', 'inserted_value_after').decode('utf-8') == '4', "LINSERT AFTER failed"
        lrange_result = r.lrange('list_key', 0, -1)
        assert [val.decode('utf-8') for val in lrange_result] == ['list_value1', 'inserted_value_before', 'list_value3', 'inserted_value_after'], "LINSERT verification failed"
        print("LINSERT test passed")
        assert r.lset('list_key', 1, 'lset_value').decode('utf-8') == True, "LSET failed"
        assert r.lrange('list_key', 0, -1)[1].decode('utf-8') == 'lset_value', "LSET verification failed"
        print("LSET test passed")
        assert r.ltrim('list_key', 1, 2).decode('utf-8') == True, "LTRIM failed"
        lrange_result = r.lrange('list_key', 0, -1)
        assert [val.decode('utf-8') for val in lrange_result] == ['lset_value', 'list_value3'], "LTRIM verification failed"
        print("LTRIM test passed")
        assert r.lrem('list_key', 1, 'list_value3') == 1, "LREM count 1 failed"
        assert r.lrem('list_key', 0, 'non_existent_value') == 0, "LREM count 0 (non-existent) failed"
        lrange_result = r.lrange('list_key', 0, -1)
        assert [val.decode('utf-8') for val in lrange_result] == ['lset_value'], "LREM verification failed"
        print("LREM test passed")
        r.rpush('source_list', ['rpoplpush_value'])
        assert r.rpoplpush('source_list', 'destination_list').decode('utf-8') == 'rpoplpush_value', "RPOPLPUSH failed"
        assert r.llen('source_list') == 0, "RPOPLPUSH source list verification failed"
        assert r.llen('destination_list') == 1, "RPOPLPUSH destination list verification failed"
        print("RPOPLPUSH test passed")

        # BLPOP and BRPOP need more complex testing due to blocking nature, skipping for basic script

        # --- Generic Commands ---
        print("\n--- Testing Generic Commands ---")
        assert r.type('string_key').decode('utf-8') == 'string', "TYPE string failed"
        assert r.type('hash_key').decode('utf-8') == 'hash', "TYPE hash failed"
        assert r.type('list_key').decode('utf-8') == 'list', "TYPE list failed"
        assert r.type('non_existent_key').decode('utf-8') == 'none', "TYPE none failed"
        print("TYPE test passed")
        r.set('exists_key', 'exists_value')
        assert r.exists('exists_key') == True, "EXISTS (existing key) failed"
        assert r.exists('non_existent_key') == False, "EXISTS (non-existent key) failed"
        print("EXISTS test passed")
        r.set('del_key', 'del_value')
        assert r.delete('del_key') == 1, "DEL (existing key) failed"
        assert r.delete('del_key') == 0, "DEL (non-existent key) failed"
        print("DEL test passed")
        r.set('key1_keys', 'value1')
        r.set('key2_keys', 'value2')
        keys_result = r.keys('key*_keys')
        assert sorted([k.decode('utf-8') for k in keys_result]) == sorted(['key1_keys', 'key2_keys']), "KEYS failed"
        print("KEYS test passed")
        r.set('expire_key', 'expire_value')
        assert r.expire('expire_key', 1) == True, "EXPIRE failed"
        assert r.ttl('expire_key') == 1, "TTL after EXPIRE incorrect"
        time.sleep(2)
        assert r.get('expire_key') == None, "EXPIRE key not expired"
        print("EXPIRE and TTL tests passed")
        r.set('persist_key', 'persist_value', ex=10)
        assert r.persist('persist_key') == True, "PERSIST failed"
        assert r.ttl('persist_key') == -1, "PERSIST TTL incorrect"
        print("PERSIST test passed")

        # --- Command Introspection Commands ---
        print("\n--- Testing Command Introspection Commands ---")
        command_result = r.command() # Basic COMMAND
        assert isinstance(command_result, list), "COMMAND result is not a list"
        assert "PING" in [cmd[0].decode('utf-8').upper() for cmd in command_result], "COMMAND does not list PING"
        print("COMMAND (basic) test passed")

        command_list_result = r.command_list()
        assert isinstance(command_list_result, list), "COMMAND LIST result is not a list"
        assert b"ping" in command_list_result, "COMMAND LIST does not list PING"
        print("COMMAND LIST test passed")

        command_info_result = r.command_info('PING')
        assert isinstance(command_info_result, list), "COMMAND INFO PING result is not a list"
        assert command_info_result[0]['name'].decode('utf-8').upper() == "PING", "COMMAND INFO PING name incorrect"
        print("COMMAND INFO PING test passed")

        command_help_result = r.command_help('SET')
        assert isinstance(command_help_result, list), "COMMAND HELP SET result is not a list"
        assert any(b'SET key value' in item for item in command_help_result), "COMMAND HELP SET does not contain 'SET key value'" # Basic check for help text
        print("COMMAND HELP SET test passed")


        print("\n--- All tests passed successfully! ---")

    except redis.exceptions.ConnectionError as e:
        print(f"Connection Error: {e}")
        print("Please ensure the Calod server is running on localhost:6379")
        pytest.fail("Connection to Calod server failed") # Fail pytest if connection error
    except AssertionError as e:
        print(f"Assertion failed: {e}")
        pytest.fail(str(e)) # Fail pytest on assertion failure
    except Exception as e:
        print(f"An unexpected error occurred: {e}")
        pytest.fail(f"Unexpected error: {e}") # Fail pytest on unexpected error


if __name__ == "__main__":
    test_calod_server_redis_compatibility()